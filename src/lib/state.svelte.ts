import { convertFileSrc } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { confirm, save as saveDialog } from '@tauri-apps/plugin-dialog';
import { SvelteMap } from 'svelte/reactivity';
import { ipc } from './ipc';
import {
	DEFAULT_NEXT_EDIT_BASE_URL,
	DEFAULT_NEXT_EDIT_HF_REPO,
	DEFAULT_NEXT_EDIT_SERVER_PORT,
	defaultEditorConfig,
	formatError,
	type AppState,
	type EditorConfig,
	type FolderSession,
	type GitBranchInfo,
	type GitFileBlame,
	type GitStatusEntry,
	type LspDiagnostic,
	type LspDiagnosticsEvent,
	type LspStatusEvent,
	type NextEditProbeResult,
	type NextEditServerSnapshot,
	type SplitSide,
	type SystemTheme,
	type ThemeMode,
	type Workspace,
	type WorkspaceFolder,
	type WorkspaceSession,
} from './protocol';
import { lspLanguageFor } from './editor/lspLanguage';
import { bottomPanel } from './bottomPanel.svelte';
import { composeLogs } from './composeLogs.svelte';
import { terminal } from './terminal.svelte';
import { coder } from './coder.svelte';
import { container } from './container.svelte';
import { canOpenContainerTerminal, openContainerTerminal, openHostTerminal } from './openTerminal';
import { projectCompose } from './projectCompose.svelte';
import { rightPanel } from './rightPanel.svelte';
import { slack } from './slack.svelte';
import { fingerprint, fingerprintEquals, type ContentFingerprint } from './util/hash';
import { fileKindFor, type FileKind } from './util/fileKind';
import { isMarkdownPath } from './util/markdown';

export type MarkdownView = 'source' | 'preview';

export type { SplitSide } from './protocol';

/**
 * One non-empty selection inside the active editor. Carries the
 * path + line range + the captured text so consumers (the coder
 * composer's "add to chat" affordance) can build a stable
 * attachment that doesn't change when the user edits the file.
 *
 * Lines are 1-based and inclusive — same shape as `read_file`'s
 * `start_line` / `end_line` arguments and `grep` hits, so a
 * Cursor-style `@filename:start-end` reference round-trips
 * straight into the agent's tool surface without conversion.
 */
export type EditorSelectionSnapshot = {
	path: string;
	startLine: number;
	endLine: number;
	text: string;
};

/**
 * One slot in the navigation history. Captures enough to re-establish
 * both the editor's active buffer and the caret inside it.
 *
 * `folder` is the absolute host path of the bound folder — `.path` is
 * folder-relative the same way `openFiles` paths are. The pair acts
 * as the stable identity of a position across folder switches and
 * tab close/re-open cycles.
 *
 * UTF-16 encoding for `line` / `character` to match LSP + CodeMirror.
 */
export type NavEntry = {
	folder: string;
	path: string;
	line: number;
	character: number;
};

function navKey(folder: string, path: string): string {
	return `${folder}::${path}`;
}

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
	// True for buffers opened against a working-tree-deleted path:
	// the bytes aren't on disk, `text` holds the `HEAD` content, and
	// the UI renders the diff view unconditionally (a plain editor
	// couldn't save back — there's nowhere to save to). Persisted in
	// the session so a deleted row the user left open pops back up
	// in diff mode after a restart.
	isDeleted: boolean;
	// True for buffers opened from outside every bound folder via
	// `openHostFile` (Ctrl+O on a path the active folder doesn't
	// contain). The bytes live wherever the host filesystem says,
	// so reads / writes route through the host-direct IPC pair
	// (`fs.readFileHost` / `fs.writeFileHost`) and the buffer skips
	// LSP, editorconfig, git, and session persistence — all of
	// those assume the file lives inside the active folder's host.
	// Path is the absolute host path; tabs render the basename.
	isExternal: boolean;
};

/**
 * Per-folder slice of UI state. One `FolderState` per folder bound
 * into the workspace — the user's tab strip, active file, split
 * layout, and untitled-buffer counter all swap when the active
 * folder changes. The workspace-wide things (theme, editorconfig
 * cache, focus tickers, toast) stay on `WorkspaceState`.
 *
 * Tab paths are folder-relative — same as in Phase 0–2's single-folder
 * world; multi-folder just gives each folder its own copy.
 */
class FolderState {
	paths = $state<string[]>([]);

	// Loaded text/image buffers, keyed by folder-relative path. A
	// buffer is shared across panes — typing in pane A updates the same
	// `OpenFile` that pane B is rendering, so the dirty marker and text
	// stay in lockstep. A buffer is dropped from this list when it
	// falls out of every pane's tab list (`closeFile` does the GC).
	openFiles = $state<OpenFile[]>([]);

	// Per-pane tab order. The two lists are independent (VSCode/Zed
	// convention): a path can live in one pane, both, or neither, and
	// reordering on one strip never touches the other.
	leftTabs = $state<string[]>([]);
	rightTabs = $state<string[]>([]);

	// Primary and (optional) secondary editor each track their own
	// active path. Phase 1 is two-pane only.
	leftActive = $state<string | null>(null);
	rightActive = $state<string | null>(null);
	hasSplit = $state(false);
	focusedSide = $state<SplitSide>('left');

	// Source vs. rendered Preview, scoped to the buffer (not the pane:
	// each path gets one mode shared across panes — same as the buffer
	// itself). Per folder so a `README.md` in folder A and folder B
	// keep independent toggles.
	previewModes = $state<Map<string, MarkdownView>>(new Map());

	// "Show the diff view instead of the editor" toggle, scoped to
	// the buffer (cross-pane consistent — same path in both splits
	// agrees on the mode). Per folder for the same isolation reason
	// as `previewModes`. Transient by design: cleared on close /
	// folder swap / session restore (we don't persist it; diff mode
	// is a glance gesture, not a property of the file).
	diffModes = $state<Set<string>>(new Set());

	// Per-folder counter for `Untitled-N` IDs. Per-folder so each
	// folder's untitled sequence starts at 1 — independent of how
	// many untitled buffers any other folder has produced.
	untitledCounter = $state(0);

	constructor(public readonly folderPath: string) {}
}

class WorkspaceState {
	workspace = $state<Workspace | null>(null);

	// Per-folder UI state. Keyed by absolute folder path — same as
	// `Workspace.active_folder` and `WorkspaceFolder.path`. Survives
	// folder switches: switching to a folder whose state already
	// exists rebinds the proxied accessors below to that folder's
	// buffers / tabs without rebuilding them.
	folderStates = new SvelteMap<string, FolderState>();

	loadingPaths = $state(false);
	toast = $state<string | null>(null);

	// False until the first paint has a reliable workspace shape + the
	// applied theme. App.svelte uses this to show a splash instead of
	// flashing the Welcome screen while `restoreAppState` is still
	// settling the OS theme probe and reading `state.json`. Flips true
	// once those two are done; session / tab restoration continues
	// afterwards in the background (empty panes fill in as files load).
	hydrated = $state(false);

	// Per-machine UI theme. Persisted alongside the session in AppState.
	// There is no project-level theme override; if a workspace ever needs
	// one, that'd live in `.editorconfig` extensions, not here.
	//
	// `'system'` is the stored value; `effectiveTheme` resolves it to
	// `'dark'` / `'light'` by reading `prefers-color-scheme`. Consumers
	// that paint pixels (editor, terminal, CSS class on `:root`) should
	// read the effective value, not this one.
	theme = $state<ThemeMode>('system');

	// Current OS preference. Updated by a `matchMedia` listener wired
	// in `restoreAppState` so a system-wide dark-mode flip repaints the
	// IDE without any further input. Only consulted when
	// `theme === 'system'`.
	systemPrefersDark = $state(detectSystemPrefersDark());

	// Local autocomplete (llama.cpp / Sweep-style model). Persisted in AppState; probe runs on a timer.
	nextEditExternalBaseUrl = $state('');
	nextEditLlamaBinary = $state('');
	nextEditHfRepo = $state(DEFAULT_NEXT_EDIT_HF_REPO);
	nextEditServerHost = $state('127.0.0.1');
	nextEditServerPort = $state(DEFAULT_NEXT_EDIT_SERVER_PORT);
	nextEditProbe = $state<NextEditProbeResult | null>(null);
	nextEditProbeInFlight = $state(false);
	/** True while a local autocomplete `/completion` request is in flight. */
	autocompleteInFlight = $state(false);
	nextEditServerSnapshot = $state<NextEditServerSnapshot | null>(null);
	nextEditServerActionInFlight = $state(false);
	/** Managed llama-server only: persisted; IDE launch may auto-start. */
	nextEditServerAutostart = $state(false);

	// Resolved theme actually used to paint the UI. Reads
	// `systemPrefersDark` when the user's choice is `'system'`, and
	// their explicit choice otherwise. Editor / terminal / the `.light`
	// class on `:root` all key off this.
	effectiveTheme: 'dark' | 'light' = $derived.by(() => {
		if (this.theme === 'system') {
			return this.systemPrefersDark ? 'dark' : 'light';
		}
		return this.theme;
	});

	// Resolved `.editorconfig` per open file. Populated lazily when a
	// file is opened and refreshed when the user saves a `.editorconfig`
	// (which invalidates server-side, then we refetch every entry). Map
	// is treated as immutable for reactivity — replace the whole thing
	// on update, never mutate in place.
	//
	// Workspace-wide rather than per-folder: `editorconfig_for_path`
	// is folder-relative on the wire today, but file paths are unique
	// per folder so the cross-folder cache stays consistent.
	editorConfigs = $state<Map<string, EditorConfig>>(new Map());

	// LSP diagnostics per path. Full-replacement semantics: each
	// `lsp:diagnostics` event overwrites the entry for its path with
	// the server's new truth, matching how language servers model
	// `publishDiagnostics`. An empty array means "server has run on
	// this file, clean slate" — distinct from "not present" which
	// means "server hasn't reported yet". The editor binds its lint
	// gutter to this; the status bar reads the active path's entry.
	diagnostics = $state<Map<string, LspDiagnostic[]>>(new Map());

	// Per-file git blame, indexed by line. The inline current-line
	// annotation and its hover tooltip read from here. Populated on
	// demand when a file is opened; refreshed on save so a freshly-
	// committed line shows the new commit's metadata without a
	// refresh.
	//
	// Scoped to the active folder by convention (the cache is
	// cleared on folder swap, same deal as `diagnostics`). Stale
	// entries after a rename are pruned via `rebindBlameForRename`.
	blameByPath = $state<Map<string, GitFileBlame>>(new Map());
	// Per-file coalescing timers for the post-save blame refresh.
	// A Rust commit that edits the target file can trigger two or
	// three saves in a second (pre-save pipeline, editorconfig tweak,
	// then the user's Ctrl+S); batching means one `git blame`
	// subprocess instead of a cascade.
	#blameTimers: Map<string, ReturnType<typeof setTimeout>> = new Map();
	// Paths whose blame IPC is currently outstanding. Prevents a
	// redundant second fetch when both `openFile` (first load) and
	// `setActive` (tab focus) fire in the same microtask, or when a
	// file is open in both splits and each Editor instance triggers
	// its own refresh. Cleared by `refreshBlame` itself on resolve/
	// reject.
	#blameInFlight: Set<string> = new Set();
	// Per-file `HEAD` blob contents, keyed by workspace-relative
	// path. Feeds the CodeMirror gutter that paints
	// added/modified/deleted wedges against the last commit. `null`
	// in the map means "we asked and there's nothing in HEAD"
	// (untracked, outside a repo, binary) — distinct from absent,
	// which means "we haven't asked yet". Lifecycle: cleared on
	// folder swap, evicted on close, seeded on open, and re-fetched
	// whenever `refreshGitStatus` runs (external commits /
	// checkouts, tree refresh, window focus).
	headByPath = $state<Map<string, string | null>>(new Map());
	// In-flight guard for `refreshHead` — `openFile` and `setActive`
	// can fire in the same tick, and a buffer open in both splits
	// would otherwise kick two parallel `git show` subprocesses
	// against the same blob.
	#headInFlight: Set<string> = new Set();
	// Per-language server state, keyed by LSP language id (`'typescript'`,
	// later `'rust'` / `'svelte'` / …). Populated by `lsp:status`
	// broker events. The status bar renders one pill per entry whose
	// state is anything other than `'running'`; `'notavailable'`
	// specifically surfaces as "install the server" guidance.
	lspStatuses = $state<Map<string, LspStatusEvent>>(new Map());
	/** Guards against re-subscribing to `lsp:*` events. */
	#lspListenersWired = false;
	/**
	 * Per-file coalescing timers for `textDocument/didChange`. Typing
	 * is bursty; we batch updates into one IPC per ~150ms tick so the
	 * server isn't chasing each keystroke.
	 */
	#lspUpdateTimers: Map<string, ReturnType<typeof setTimeout>> = new Map();

	// Monotonic counter the active editor view watches to refocus itself.
	// Bumped whenever the user "navigates" to a file (tab click, tree click,
	// post-close fallback). The Editor component reads it as a reactive
	// dependency and calls `view.focus()` on every change. Keeping focus
	// lives here — not in Editor.svelte — so non-editor surfaces (file tree,
	// command palette, future shortcuts) can request it uniformly.
	focusTick = $state(0);
	/** Bumped with [`requestAutocomplete`] so the focused editor runs autocomplete (Ctrl+T). */
	autocompleteEditorTick = $state(0);
	// Sibling tickers for the sidebar (file tree) and status bar. F6 /
	// Ctrl+0 / Esc-from-tree all just bump these and the relevant
	// component pulls focus in. Same pattern as `focusTick`; keeping
	// the tickers in WorkspaceState (rather than passing component
	// refs around) lets every region focus-shift call site stay
	// declarative.
	sidebarFocusTick = $state(0);
	statusFocusTick = $state(0);

	// Linear navigation history, browser-style but position-aware (each
	// entry pins a caret inside a file rather than just a path).
	//
	// Entries carry the absolute `folder` path so history works across
	// multi-folder workspaces: navigating back to an entry in folder B
	// while folder A is active switches folders first. `path` is
	// folder-relative — the same form `openFiles` / tabs use.
	//
	// Two mutation modes:
	//
	// 1. **Push** (append + truncate forward): clicks (`select.pointer`
	//    transactions) and file switches create a new entry and bump
	//    `navIndex`. Opening a file while not at the tip truncates the
	//    forward stack — same as a browser URL bar.
	// 2. **Update in place** (tip tracking): keyboard arrow motion,
	//    selection extension, programmatic selection changes — none of
	//    these are "navigations", but the *last-known caret* per entry
	//    has to track them or Alt+Right forward-navigate would land at
	//    an old position the user moved away from.
	//
	// Matches VS Code's feel: clicks and file switches are history
	// events; arrow keys and typing just drag the tip along. A small
	// threshold + same-file coalescing prevents accidental double
	// entries when a click barely shifts the caret.
	navStack: NavEntry[] = $state([]);
	navIndex = $state(-1);
	// Guard flag: set while we're the ones driving an openFile (from
	// navigateBack / navigateForward / jumpTo) so the standard push
	// path doesn't record the very navigation we just performed.
	// Cleared by the method that sets it.
	private suppressNavPush = false;

	// One-shot carets the Editor component consumes on its next render
	// for a given (folder, path) key. `jumpTo` / `navigateBack` /
	// `navigateForward` set an entry; the Editor's `$effect` watching
	// this map dispatches the selection-change + scroll-into-view and
	// removes the entry.
	//
	// Key is `"${folder}::${path}"` rather than just `path` because
	// folder A and folder B can each have a `src/lib.rs` open in a
	// multi-folder workspace; using a bare path would cross the
	// streams.
	pendingJumps: SvelteMap<string, { line: number; character: number }> = $state(new SvelteMap());

	// Persistence guards. `persistScheduled` coalesces bursts of mutations
	// (e.g. closeFile mutates openFiles + leftActive in the same tick) into
	// a single IPC roundtrip. `suppressPersist` is set during startup
	// restore so we don't round-trip the freshly-loaded state right back
	// to disk on every `openFile` call.
	private persistScheduled = false;
	private suppressPersist = false;

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

	/**
	 * Active folder path, derived from the workspace shape. `null`
	 * when the workspace has no folders or no active folder. Updates
	 * whenever the backend snapshot lands on `this.workspace`.
	 */
	activeFolderPath: string | null = $derived(this.workspace?.active_folder ?? null);

	/**
	 * Active folder record. Components reach for `.path`, `.name`,
	 * `.host` here instead of the workspace-level fields the
	 * single-folder shape used to expose.
	 */
	activeFolder: WorkspaceFolder | null = $derived.by(() => {
		const ws = this.workspace;
		if (!ws || ws.active_folder === null) {
			return null;
		}
		return ws.folders.find((f) => f.path === ws.active_folder) ?? null;
	});

	/**
	 * Active folder's reactive state. All the per-folder accessor
	 * proxies below route through this. `null` when no folder is
	 * active — proxied reads return defaults, proxied writes are
	 * silent no-ops.
	 */
	private activeFolderState: FolderState | null = $derived.by(() => {
		const path = this.activeFolderPath;
		if (path === null) {
			return null;
		}
		return this.folderStates.get(path) ?? null;
	});

	// Per-folder accessor proxies. Components (and most methods on
	// this class) keep reaching for `workspace.openFiles` etc. — those
	// reads transparently forward to the active folder's `FolderState`
	// and writes mutate that state in place. Reactivity flows because
	// the underlying fields are `$state` on `FolderState`.

	get paths(): string[] {
		return this.activeFolderState?.paths ?? [];
	}
	set paths(value: string[]) {
		if (this.activeFolderState) {
			this.activeFolderState.paths = value;
		}
	}

	get openFiles(): OpenFile[] {
		return this.activeFolderState?.openFiles ?? [];
	}
	set openFiles(value: OpenFile[]) {
		if (this.activeFolderState) {
			this.activeFolderState.openFiles = value;
		}
	}

	get leftTabs(): string[] {
		return this.activeFolderState?.leftTabs ?? [];
	}
	set leftTabs(value: string[]) {
		if (this.activeFolderState) {
			this.activeFolderState.leftTabs = value;
		}
	}

	get rightTabs(): string[] {
		return this.activeFolderState?.rightTabs ?? [];
	}
	set rightTabs(value: string[]) {
		if (this.activeFolderState) {
			this.activeFolderState.rightTabs = value;
		}
	}

	get leftActive(): string | null {
		return this.activeFolderState?.leftActive ?? null;
	}
	set leftActive(value: string | null) {
		if (this.activeFolderState) {
			this.activeFolderState.leftActive = value;
		}
	}

	get rightActive(): string | null {
		return this.activeFolderState?.rightActive ?? null;
	}
	set rightActive(value: string | null) {
		if (this.activeFolderState) {
			this.activeFolderState.rightActive = value;
		}
	}

	get hasSplit(): boolean {
		return this.activeFolderState?.hasSplit ?? false;
	}
	set hasSplit(value: boolean) {
		if (this.activeFolderState) {
			this.activeFolderState.hasSplit = value;
		}
	}

	get focusedSide(): SplitSide {
		return this.activeFolderState?.focusedSide ?? 'left';
	}
	set focusedSide(value: SplitSide) {
		if (this.activeFolderState) {
			this.activeFolderState.focusedSide = value;
		}
	}

	private get previewModes(): Map<string, MarkdownView> {
		return this.activeFolderState?.previewModes ?? new Map();
	}
	private set previewModes(value: Map<string, MarkdownView>) {
		if (this.activeFolderState) {
			this.activeFolderState.previewModes = value;
		}
	}

	private get diffModes(): Set<string> {
		return this.activeFolderState?.diffModes ?? new Set();
	}
	private set diffModes(value: Set<string>) {
		if (this.activeFolderState) {
			this.activeFolderState.diffModes = value;
		}
	}

	private get untitledCounter(): number {
		return this.activeFolderState?.untitledCounter ?? 0;
	}
	private set untitledCounter(value: number) {
		if (this.activeFolderState) {
			this.activeFolderState.untitledCounter = value;
		}
	}

	activePath: string | null = $derived(this.focusedSide === 'left' ? this.leftActive : this.rightActive);

	activeFile: OpenFile | null = $derived.by(() => {
		if (this.activePath === null) {
			return null;
		}
		return this.openFiles.find((f) => f.path === this.activePath) ?? null;
	});

	/**
	 * Snapshot of the *non-empty* selection in the active editor,
	 * if any. Updated by `Editor.svelte`'s selection listener and
	 * cleared on any of: empty selection, file switch, focus loss
	 * to a non-editor surface. Read by:
	 *
	 * - The `Ctrl+L` keymap, to attach the selected range to the
	 *   coder composer ("add to chat" gesture, mirrors Cursor).
	 * - The editor pane's floating "Add to Coder" hint pill.
	 *
	 * `text` is captured at update time so a follow-up edit to
	 * the file doesn't change the agent context — what the user
	 * attached is what the agent sees.
	 */
	activeSelection = $state<EditorSelectionSnapshot | null>(null);

	setActiveSelection(snapshot: EditorSelectionSnapshot | null): void {
		// Cheap stable equality: same path + same line range +
		// same length text → no state mutation. The selection
		// listener fires on every keystroke even when the
		// selection didn't change (drag-select hits the same
		// range repeatedly), and we don't want every keystroke to
		// re-trigger reactive consumers.
		const a = this.activeSelection;
		if (a === null && snapshot === null) {
			return;
		}
		if (
			a !== null &&
			snapshot !== null &&
			a.path === snapshot.path &&
			a.startLine === snapshot.startLine &&
			a.endLine === snapshot.endLine &&
			a.text === snapshot.text
		) {
			return;
		}
		this.activeSelection = snapshot;
	}

	/**
	 * Add `path` as a folder in the workspace and make it active.
	 * Idempotent on duplicate path — the backend silently flips the
	 * existing entry to active, and we re-load its tree if it had
	 * never been populated. Per Phase 2.5 this is the single
	 * "open folder" code path: the welcome screen, the sidebar's
	 * `+ Add folder` row, the command palette's "Open Folder…", and
	 * the `EditorPane` empty-state button all funnel through here.
	 */
	async openLocal(path: string) {
		const beforeCount = this.workspace?.folders.length ?? 0;
		try {
			const ws = await ipc.workspace.openLocal(path);
			await this.adoptWorkspaceSnapshot(ws);
			this.persistAppState();
			// Backend's `add_folder` is idempotent on duplicate paths
			// (and we *want* it to flip active in that case). The folder
			// count is the cleanest "did we actually add a new folder?"
			// signal — paths the user picked may not equal what the
			// backend canonicalised, so a string compare is unreliable.
			if (ws.folders.length === beforeCount) {
				this.flash(`Folder is already in the workspace.`);
				return;
			}
			// New folder bound: re-emit `compose.yaml` and (if the
			// project is running) apply the diff via
			// `compose up -d --wait`. The backend treats the no-op
			// case (no compose.yaml yet) as a free pass, so this is
			// cheap pre-opt-in.
			void container.syncBoundFolders();
		} catch (err) {
			this.flash(`Failed to open: ${formatError(err)}`);
		}
	}

	/**
	 * Set `path` as the active folder. Tab strip + tree swap to that
	 * folder's persisted state without losing the previous folder's
	 * buffers.
	 */
	async setActiveFolder(path: string) {
		if (this.activeFolderPath === path) {
			return;
		}
		try {
			const ws = await ipc.workspace.setActiveFolder(path);
			await this.adoptWorkspaceSnapshot(ws);
			this.persistAppState();
		} catch (err) {
			this.flash(`Could not switch folder: ${formatError(err)}`);
		}
	}

	/**
	 * Drop a folder from the workspace. Confirms first; bails if the
	 * folder has any dirty buffers and the user declines to discard.
	 * The folder's `FolderState` (and every buffer in it) is dropped
	 * — those tabs were exclusive to the folder.
	 */
	async removeFolder(path: string) {
		const folder = this.workspace?.folders.find((f) => f.path === path);
		if (!folder) {
			return;
		}
		const folderState = this.folderStates.get(path);
		const dirty = folderState?.openFiles.filter((f) => f.isDirty) ?? [];
		const ok = await confirm(
			dirty.length > 0
				? `Remove ${folder.name} from the workspace?\n\n${dirty.length} unsaved buffer(s) will be discarded.`
				: `Remove ${folder.name} from the workspace?`,
			{
				title: 'Remove folder',
				okLabel: 'Remove',
				cancelLabel: 'Cancel',
			},
		);
		if (!ok) {
			return;
		}
		try {
			const ws = await ipc.workspace.removeFolder(path);
			this.folderStates.delete(path);
			this.pruneNavEntriesForFolder(path);
			// Drop the folder's compose snapshot from the per-folder
			// store. We don't `compose down` its services on the
			// daemon — the user may have removed the folder for a
			// session purely to declutter the sidebar, and tearing
			// down running services as a side effect would surprise.
			// They still show up in `docker compose ls` and can be
			// torn down by re-binding the folder if needed.
			projectCompose.forget(path);
			await this.adoptWorkspaceSnapshot(ws);
			this.persistAppState();
			// Bound-folder set shrunk: re-emit `compose.yaml` so the
			// dev shell drops the now-unmounted folder on its next
			// `up -d --wait` cycle.
			void container.syncBoundFolders();
		} catch (err) {
			this.flash(`Could not remove folder: ${formatError(err)}`);
		}
	}

	/**
	 * Apply a freshly returned `Workspace` snapshot from the backend:
	 * update `this.workspace`, ensure each bound folder has a
	 * matching `FolderState`, and (re)load the active folder's tree
	 * if it isn't already populated.
	 *
	 * Public because `App.svelte`'s startup hydrate reaches for it
	 * after the backend's first `workspace_active` returns the
	 * replayed shape — same code path mutating commands take, just
	 * from the launch flow rather than a user gesture.
	 *
	 * Does **not** persist on its own — caller is responsible. User
	 * gestures (open / switch / remove) call `persistAppState()`
	 * afterwards; the launch hydrate skips it because
	 * `restoreAppState()` is about to overwrite the on-disk session
	 * with the *replayed* shape and a premature persist here would
	 * race with the load and erase the session before we read it.
	 *
	 * Container syncing (rewriting `compose.yaml`, applying via
	 * `compose up -d --wait` if running) is **not** triggered here
	 * — folder switches don't change the bound-folder set. Add /
	 * remove call sites are responsible for nudging
	 * `container.syncBoundFolders()` after they persist.
	 *
	 * Empty snapshot (no folders) collapses to the welcome-screen
	 * state.
	 */
	async adoptWorkspaceSnapshot(snapshot: Workspace) {
		// A folder swap invalidates the per-path blame cache: the
		// same relative path can mean different files in folders A
		// and B (both `src/lib.rs` in a multi-folder workspace), and
		// even when it's the same file the commit history is
		// scoped to the folder's repo. Drop the cache and let the
		// opened-file effects refetch for the new folder.
		const previousActive = this.workspace?.active_folder ?? null;
		if (previousActive !== null && previousActive !== snapshot.active_folder) {
			for (const timer of this.#blameTimers.values()) {
				clearTimeout(timer);
			}
			this.#blameTimers.clear();
			this.blameByPath = new Map();
			this.headByPath = new Map();
			this.#headInFlight.clear();
		}
		this.workspace = snapshot.folders.length === 0 ? null : snapshot;
		// Tell the coder panel which folder is now active so its
		// per-folder UI bucket flips. Per the multi-session design:
		// turns running in the previous folder keep going in the
		// background, just streaming events into their own bucket.
		// The user sees the new folder's transcript / sessions list
		// / draft / attachments restored intact when they return.
		coder.setActiveFolder(snapshot.active_folder ?? null);
		// Drop FolderStates whose folders aren't bound anymore. Two-pass
		// (collect-then-delete) so we never mutate the map while
		// iterating — the spec allows it, but oxlint flags the spread
		// alternative and the explicit two-pass is clearer about intent.
		const drop: string[] = [];
		const bound = new Set(snapshot.folders.map((f) => f.path));
		for (const path of this.folderStates.keys()) {
			if (!bound.has(path)) {
				drop.push(path);
			}
		}
		for (const path of drop) {
			this.folderStates.delete(path);
		}
		// Allocate FolderStates for any new folders.
		for (const folder of snapshot.folders) {
			if (!this.folderStates.has(folder.path)) {
				this.folderStates.set(folder.path, new FolderState(folder.path));
			}
		}
		// Hydrate the active folder's tree if it's a fresh state.
		if (this.activeFolderState && this.activeFolderState.paths.length === 0) {
			await this.loadPaths();
		}
		// Warm the per-folder compose snapshot for every bound
		// folder so the bars paint with real data on first frame
		// (rather than blank-then-flash). Cheap when most folders
		// have no compose.yaml — the backend short-circuits to
		// `Absent` without invoking docker.
		void projectCompose.refreshAll(snapshot.folders.map((f) => f.path));
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
		// Probe the OS theme (XDG portal / native API) and read the
		// persisted `state.json` in parallel — they're independent
		// and both block the first meaningful paint. `bindSystemPreference`
		// is awaited so `applyTheme` below reads a trustworthy
		// `systemPrefersDark`: on Linux WebKitGTK the synchronous
		// `matchMedia` answer seeded during class construction is
		// unreliable, and only the portal probe knows for sure.
		const appStatePromise = ipc.appState.load().then(
			(state) => ({ ok: true, state }) as const,
			(err) => ({ ok: false, err }) as const,
		);
		await this.bindSystemPreference();
		const appStateResult = await appStatePromise;
		if (!appStateResult.ok) {
			this.flash(`Could not restore state: ${formatError(appStateResult.err)}`);
			applyTheme(this.effectiveTheme);
			this.hydrated = true;
			return;
		}
		const state = appStateResult.state;

		this.theme = state.theme;
		this.nextEditExternalBaseUrl = (state.next_edit.external_base_url ?? '').trim();
		this.nextEditLlamaBinary = state.next_edit.llama_binary ?? '';
		{
			const raw = state.next_edit.hf_repo ?? '';
			const trimmed = raw.trim();
			this.nextEditHfRepo = trimmed.length > 0 ? trimmed : DEFAULT_NEXT_EDIT_HF_REPO;
		}
		this.nextEditServerHost =
			state.next_edit.server_host?.trim().length > 0 ? state.next_edit.server_host.trim() : '127.0.0.1';
		const listenPort = state.next_edit.server_port;
		this.nextEditServerPort =
			typeof listenPort === 'number' && Number.isFinite(listenPort) && listenPort >= 1 && listenPort <= 65535
				? listenPort
				: DEFAULT_NEXT_EDIT_SERVER_PORT;
		this.nextEditServerAutostart = state.next_edit.server_autostart ?? false;
		applyTheme(this.effectiveTheme);
		// Flip `hydrated` before we start chewing on the persisted
		// session so the main layout (Welcome or editor shell) paints
		// immediately. Per-tab file reads below can take hundreds of
		// ms on a large session; no reason to gate the first paint on
		// them — empty panes fill in as each file loads.
		this.hydrated = true;
		// Restore the right-side slot pick first so chat/coder can
		// hydrate against the right baseline. The slot may have been
		// open at the last shutdown, in which case the relevant
		// surface mounts (and runs its first probe) on this same
		// paint without the user lifting a finger.
		rightPanel.hydrate(state.right_panel);
		slack.hydrate(state.slack);
		// Same for the bottom panel — visibility and height. Tab
		// contents (log streams) are not persisted by design: they
		// back onto running processes that don't survive a launch.
		// Bind the change handler before hydrating so the first user
		// interaction triggers a save.
		bottomPanel.bindOnChange(() => this.persistAppState());
		bottomPanel.hydrate(state.bottom_panel);
		// Bind Tauri push events + window-focus listener once the
		// Tauri runtime is up. Idempotent — `wireRuntime` early-returns
		// on subsequent calls (HMR-safe).
		void slack.wireRuntime();
		// Same pattern for the coder loop's `coder:event` channel.
		// Bind even when the user hasn't opened the panel yet so
		// that an in-flight turn (e.g. resumed across HMR reloads)
		// keeps streaming into `coder.rows`.
		void coder.wireRuntime();
		// Same pattern for the container status pip — bind the
		// `container:state` event subscription once, then pull the
		// current snapshot for whatever workspace is open. Capture
		// the refresh promise so the auto-terminal step at the
		// end of this method can read the resolved status rather
		// than racing with the in-flight call.
		void container.wireRuntime();
		const containerRefresh = container.refresh();
		// Per-folder compose snapshots use a parallel event
		// subscription keyed on `folder_path`. The folder bars'
		// indicators read from `projectCompose.snapshotFor(path)`,
		// which gets populated lazily via `refreshAll` whenever
		// the workspace shape changes.
		void projectCompose.wireRuntime();
		// Streamed `docker compose logs` lines come in over their
		// own event channel. Wire idempotently here so the bottom
		// panel's log tabs receive lines as soon as the user opens
		// one — no per-tab subscription dance.
		void composeLogs.wireRuntime();
		this.wireNextEditProbe();
		void this.refreshNextEditServerStatusThenMaybeAutostart();
		// Terminal output rides on its own event channel —
		// see `terminal.svelte.ts`. Wired once at startup so
		// the first `+ Terminal` click responds without bus-bind
		// latency. Hold the bind promise so the auto-terminal
		// spawn below waits for the listener to actually attach
		// instead of racing with the first PTY output bytes.
		const terminalRuntime = terminal.wireRuntime();
		// Refresh the file tree + git status when the user comes
		// back to the window. Covers the common "I ran `git
		// checkout`/`stash pop` in an external terminal" workflow
		// without requiring an fs-watcher (Phase 5). Idempotent:
		// bound once, survives HMR.
		void this.bindFolderChangeRefresh();
		// LSP event stream: we subscribe unconditionally so a
		// later `lsp_*` call (triggered by opening a TS file) has
		// a listener in place before the broker starts emitting.
		void this.bindLspListeners();

		const ws = this.workspace;
		const session = state.last_session;
		if (!ws || !session) {
			// Even without a session to replay we still want to give
			// the bottom-panel auto-spawn a shot — the panel's
			// visibility is in `state` (just hydrated above) and is
			// independent from the per-folder tab session restored
			// below.
			void this.spawnInitialBottomPanelTerminal(containerRefresh, terminalRuntime);
			return;
		}

		// Replay each persisted folder's tabs into its `FolderState`.
		// The backend has already restored the workspace shape (folder
		// list + active folder) on launch, and `App.svelte`'s hydrate
		// has populated `this.workspace`; here we only fill in the
		// per-folder UI state that lives entirely on the frontend.
		this.suppressPersist = true;
		try {
			for (const folderSession of session.folders) {
				const folder = ws.folders.find((f) => f.path === folderSession.folder_path);
				if (!folder) {
					// Persisted folder no longer in the workspace (renamed
					// / removed externally between launches). Skip.
					continue;
				}
				const fs = this.folderStates.get(folder.path);
				if (!fs) {
					continue;
				}
				const previousActive = this.activeFolderPath;
				try {
					// Temporarily redirect proxied accessors at this
					// folder so `loadTextFile` / `loadImageFile` (which
					// route through the active folder's host on the
					// backend) read from the right tree. Restored at
					// the end via the `finally`.
					await ipc.workspace.setActiveFolder(folder.path);
					const unique = new Set<string>([...folderSession.open_files_left, ...folderSession.open_files_right]);
					const loaded: OpenFile[] = [];
					for (const path of unique) {
						try {
							const kind = fileKindFor(path);
							const file = kind === 'image' ? await this.loadImageFile(path) : await this.loadTextFile(path);
							if (file) {
								loaded.push(file);
							}
						} catch {
							// File was moved/deleted since the last session.
							// Silently drop it; the post-restore
							// `persistAppState` writes the cleaned-up list
							// so it stops haunting future launches.
						}
					}
					const isLoaded = (p: string) => loaded.some((f) => f.path === p);
					fs.openFiles = loaded;
					fs.leftTabs = folderSession.open_files_left.filter(isLoaded);
					fs.rightTabs = folderSession.open_files_right.filter(isLoaded);
					// `openFile` would normally call `lspOpen` on the
					// first load, but session restore writes
					// `openFiles` directly to skip the per-file IPC
					// roundtrip. Without this catch-up loop the LSP
					// broker would never see `didOpen` for restored
					// buffers — the user would have to close + reopen
					// each tab before diagnostics started arriving.
					// Only the active folder's restore wires up the
					// LSP because the `LspBroker` is rooted at the
					// active folder, not the whole workspace; inactive
					// folders' files are loaded into memory but their
					// LSPs spawn lazily on the first folder switch.
					if (folder.path === ws.active_folder) {
						for (const file of loaded) {
							if (file.kind === 'text' && !file.isDeleted) {
								this.lspOpen(file.path, file.text);
							}
						}
					}
					const isOpenIn = (side: SplitSide, p: string | null) =>
						p !== null && (side === 'left' ? fs.leftTabs.includes(p) : fs.rightTabs.includes(p));
					fs.leftActive = isOpenIn('left', folderSession.active_left)
						? folderSession.active_left
						: (fs.leftTabs[0] ?? null);
					fs.hasSplit = folderSession.has_split && fs.rightTabs.length > 0;
					fs.rightActive =
						fs.hasSplit && isOpenIn('right', folderSession.active_right)
							? folderSession.active_right
							: fs.hasSplit
								? (fs.rightTabs[0] ?? null)
								: null;
					fs.focusedSide = folderSession.focused_side === 'right' && fs.hasSplit ? 'right' : 'left';
				} finally {
					if (previousActive !== null && previousActive !== folder.path) {
						try {
							await ipc.workspace.setActiveFolder(previousActive);
						} catch {
							// Active-folder restore failed — the backend
							// already logs; we leave whatever the loop
							// left as active rather than crash startup.
						}
					}
				}
			}
			// Restore the active-folder pointer last so the UI lands on
			// the right folder regardless of replay order.
			if (session.active_folder_path && ws.folders.some((f) => f.path === session.active_folder_path)) {
				try {
					const updated = await ipc.workspace.setActiveFolder(session.active_folder_path);
					this.workspace = updated;
				} catch {
					// Active-folder restore failed — the backend already
					// logs; the existing default-active handling stands.
				}
			}
		} finally {
			this.suppressPersist = false;
		}
		// Re-save so dropped files don't haunt the next launch, and
		// request editor focus once the active tab is in place.
		this.persistAppState();
		if (this.activePath !== null) {
			this.requestEditorFocus();
		}
		// Seed git blame for the initially-visible buffers on both
		// splits. Session restore bypasses `setActive` (it assigns
		// `leftActive` / `rightActive` directly while loading), so
		// the normal "buffer became active → fetch blame" trigger
		// never fires for the first tab. Doing it here runs after
		// the backend's active folder has been finalised by
		// `setActiveFolder(session.active_folder_path)` above, so
		// the IPC hits the right folder's host. Non-active tabs
		// fetch lazily on first click through `setActive`.
		for (const activePath of [this.leftActive, this.rightActive]) {
			if (activePath === null) {
				continue;
			}
			const file = this.openFiles.find((f) => f.path === activePath);
			if (file && file.kind === 'text') {
				this.refreshBlame(activePath);
				if (!file.isDeleted) {
					this.refreshHead(activePath);
				}
			}
		}
		// Warm the editorconfig cache for every restored tab in the
		// active folder so the initial paint already shows the right
		// indent settings.
		await Promise.all(this.openFiles.map((f) => this.ensureEditorConfig(f.path)));
		// Auto-spawn one terminal when the bottom panel was
		// restored as visible but came back without any tabs. Tab
		// contents aren't persisted by design (ADR 0009), so a
		// panel the user left open at last shutdown otherwise
		// reappears as a meaningless empty strip.
		void this.spawnInitialBottomPanelTerminal(containerRefresh, terminalRuntime);
	}

	/**
	 * Open one terminal into the bottom panel iff the panel was
	 * restored as visible but no tab kind populated it during
	 * launch. Picks `container` over `host` whenever the workspace
	 * shell is up — that's the environment the user's active folder
	 * actually runs in. Falls back to `host` otherwise (container
	 * down, paused, or workspace lacks a container project).
	 *
	 * Awaits `containerRefresh` first so the target choice reflects
	 * the daemon's current truth rather than the pre-hydrate `null`
	 * status. Awaits `terminalRuntime` so the `terminal:output`
	 * listener is attached before we spawn — otherwise the first
	 * prompt bytes would be dropped on the floor.
	 *
	 * Skips silently when there's no workspace bound (a
	 * `$HOME`-rooted host shell with no folder context isn't
	 * useful) or when something else has populated the panel
	 * between hydrate and the awaits resolving (a log stream
	 * starting itself, a follow-up gesture from the user).
	 */
	private async spawnInitialBottomPanelTerminal(
		containerRefresh: Promise<void>,
		terminalRuntime: Promise<void>,
	): Promise<void> {
		if (!this.workspace) {
			return;
		}
		if (!bottomPanel.visible) {
			return;
		}
		if (bottomPanel.tabs.length > 0) {
			return;
		}
		await containerRefresh;
		await terminalRuntime;
		if (bottomPanel.tabs.length > 0) {
			return;
		}
		if (canOpenContainerTerminal()) {
			openContainerTerminal();
			return;
		}
		openHostTerminal();
	}

	private nextEditProbeWired = false;

	private wireNextEditProbe() {
		if (this.nextEditProbeWired || typeof window === 'undefined') {
			return;
		}
		this.nextEditProbeWired = true;
		void this.refreshNextEditProbe();
		window.addEventListener('focus', () => {
			void this.refreshNextEditProbe();
		});
		window.setInterval(() => {
			void this.refreshNextEditProbe();
		}, 8000);
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
			// External buffers (Ctrl+O on a file outside every bound
			// folder) get the same treatment — the persisted session is
			// per-folder, and a file from the host's $HOME has no
			// natural folder to belong to. Re-typing Ctrl+O after a
			// restart is the contract.
			const externalSet = new Set<string>();
			for (const fs of this.folderStates.values()) {
				for (const f of fs.openFiles) {
					if (f.isExternal) {
						externalSet.add(f.path);
					}
				}
			}
			const isPersistable = (p: string) => !isUntitledPath(p) && !externalSet.has(p);
			const realPaths = (paths: string[]) => paths.filter(isPersistable);
			const realActive = (path: string | null) => (path !== null && isPersistable(path) ? path : null);
			const folderSessions: FolderSession[] = [];
			if (ws) {
				for (const folder of ws.folders) {
					const fs = this.folderStates.get(folder.path);
					if (!fs) {
						continue;
					}
					folderSessions.push({
						folder_path: folder.path,
						open_files_left: realPaths(fs.leftTabs),
						open_files_right: fs.hasSplit ? realPaths(fs.rightTabs) : [],
						active_left: realActive(fs.leftActive),
						active_right: fs.hasSplit ? realActive(fs.rightActive) : null,
						has_split: fs.hasSplit,
						focused_side: fs.focusedSide,
					});
				}
			}
			const session: WorkspaceSession | null = ws
				? { folders: folderSessions, active_folder_path: ws.active_folder }
				: null;
			// `slack`, `right_panel`, and `coder` are written through
			// their own Tauri commands (`slack_*`, `ui_set_right_panel`,
			// `coder_*`); the backend's `app_state_save` ignores
			// whatever we send for those fields and preserves the
			// on-disk value. The placeholders satisfy the shared type
			// only.
			const payload: AppState = {
				last_session: session,
				theme: this.theme,
				slack: { active_bot: null, active_thread_ts: null },
				bottom_panel: bottomPanel.serialise(),
				right_panel: null,
				coder: { last_session_by_folder: {} },
				next_edit: {
					external_base_url: this.nextEditExternalBaseUrl.trim(),
					llama_binary: this.nextEditLlamaBinary.trim(),
					hf_repo: this.nextEditHfRepo.trim(),
					server_host: this.nextEditServerHost.trim() || '127.0.0.1',
					server_port: this.nextEditServerPort,
					server_autostart: this.nextEditServerAutostart,
				},
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

	async loadPaths(changedSubset: ReadonlySet<string> | null = null) {
		if (!this.activeFolder) {
			return;
		}
		this.loadingPaths = true;
		try {
			// One IPC call, full recursive walk backend-side. The
			// previous implementation fired one `readDir` per
			// directory which at Tauri's per-call framing cost
			// dominated refresh latency (the walk itself is a
			// sub-hundred-ms `read_dir` storm).
			const collected = await ipc.fs.collectPaths(MAX_TREE_DEPTH);
			this.paths = collected;
			// Classify git status in the background — the tree can
			// paint before we know the answer. Pierre reconciles
			// `setGitStatus` updates in place, so late-arriving
			// entries fade / tint the affected rows without a reflow.
			void this.refreshGitStatus(collected, changedSubset);
		} catch (err) {
			this.flash(`Failed to read folder: ${formatError(err)}`);
		} finally {
			this.loadingPaths = false;
		}
	}

	/**
	 * Per-path git classification, as reported by
	 * `WorkspaceHost::git_status_entries`. Covers added / modified /
	 * deleted / untracked / ignored. `FileTree.svelte` hands this
	 * straight to Pierre's `setGitStatus`, and separately walks it
	 * for `Deleted` rows to merge back into the visible path list.
	 */
	gitStatusEntries = $state<readonly GitStatusEntry[]>([]);

	/**
	 * Active folder's branch + HEAD info. The SCM panel reads
	 * `name` for its header; `headShortSha` is the fallback the
	 * panel shows when the repo is in detached-HEAD state. Both
	 * `null` means "no branch label" — non-repo folder, repo with
	 * no commits, or git unavailable. Refreshed on folder switch,
	 * after our own commits, and on every `refreshGitStatus` pass
	 * so external `git checkout` / `git switch` from a terminal
	 * eventually surfaces (within the watcher's debounce window).
	 */
	gitBranch = $state<GitBranchInfo>({
		name: null,
		headShortSha: null,
		hasUpstream: false,
		ahead: 0,
		behind: 0,
		prUrl: null,
	});

	/**
	 * SCM-filter toggle: when on, the sidebar swaps the regular
	 * file tree for a changes-only view (only paths that appear in
	 * `gitStatusEntries` with non-`ignored` status, fully expanded).
	 * Click on a row in that view opens the file in diff mode.
	 *
	 * The toggle is per-session; not persisted in `AppState`. Two
	 * tree instances stay mounted simultaneously (CSS visibility
	 * toggle) so switching back doesn't lose the all-view's
	 * expansion / scroll state — Pierre keeps each tree's
	 * internal model intact across the visibility flip.
	 */
	scmFilterOn = $state(false);

	toggleScmFilter() {
		this.scmFilterOn = !this.scmFilterOn;
	}

	/**
	 * Paths the SCM filter view should render. Strips ignored
	 * entries (the changes view shouldn't surface
	 * `node_modules/`-style noise) and folds in deleted ghost rows
	 * the filesystem walk doesn't know about. Used by the changes-
	 * only `FileTree` instance via its `paths` reactive source.
	 */
	get scmFilterPaths(): string[] {
		const out: string[] = [];
		const seen = new Set<string>();
		for (const entry of this.gitStatusEntries) {
			if (entry.status === 'ignored') {
				continue;
			}
			if (seen.has(entry.path)) {
				continue;
			}
			seen.add(entry.path);
			out.push(entry.path);
		}
		return out;
	}

	/** Number of non-ignored entries the SCM badge should display. */
	get scmChangeCount(): number {
		let n = 0;
		for (const entry of this.gitStatusEntries) {
			if (entry.status !== 'ignored') {
				n += 1;
			}
		}
		return n;
	}

	private async refreshGitBranch() {
		try {
			this.gitBranch = await ipc.fs.gitBranch();
		} catch {
			this.gitBranch = {
				name: null,
				headShortSha: null,
				hasUpstream: false,
				ahead: 0,
				behind: 0,
				prUrl: null,
			};
		}
	}

	/**
	 * Stage every working-tree change and commit with `message`.
	 * `amend` rewrites HEAD instead of creating a new commit; an
	 * empty message in amend mode falls through to git's
	 * `--no-edit` (preserve the previous subject). The SCM panel
	 * gates this on the toggle's state. Surfaces backend errors
	 * via `flash` so the input can stay focused for a quick
	 * retry. On success, triggers a folder refresh + branch
	 * re-fetch so the tree's git status, blame, and the branch
	 * label all settle.
	 */
	async commitChanges(message: string, amend = false) {
		const trimmed = message.trim();
		if (trimmed.length === 0 && !amend) {
			this.flash('Commit message is empty.');
			return false;
		}
		try {
			const result = await ipc.fs.gitCommit(trimmed, amend);
			const verb = amend ? 'Amended' : 'Committed';
			this.flash(`${verb} ${result.shortSha}: ${result.summary}`);
			await this.refreshGitBranch();
			void this.refreshActiveFolder();
			return true;
		} catch (err) {
			this.flash(`Commit failed: ${formatError(err)}`);
			return false;
		}
	}

	/**
	 * `git push` for the active folder's current branch. Errors
	 * (no upstream, auth, non-fast-forward) surface as a flash
	 * with git's own stderr — the user gets the actionable hint
	 * verbatim. On success, refresh git status so any
	 * remote-tracking-branch indicators update.
	 */
	async pushChanges() {
		try {
			await ipc.fs.gitPush();
			this.flash('Push succeeded.');
			void this.refreshActiveFolder();
			return true;
		} catch (err) {
			this.flash(`Push failed: ${formatError(err)}`);
			return false;
		}
	}

	/**
	 * `git push -u origin HEAD` — first-push affordance for a
	 * freshly-created local branch with no upstream yet. The SCM
	 * panel shows this as "Publish branch" in the slot the sync
	 * button normally occupies. On success, refresh the branch
	 * info so `hasUpstream` flips to `true` and subsequent pushes
	 * route through `pushChanges`.
	 */
	async publishBranch() {
		try {
			await ipc.fs.gitPublishBranch();
			this.flash('Branch published.');
			await this.refreshGitBranch();
			void this.refreshActiveFolder();
			return true;
		} catch (err) {
			this.flash(`Publish failed: ${formatError(err)}`);
			return false;
		}
	}

	/**
	 * `git pull`. Failures (conflicts, dirty tree, no upstream)
	 * surface via flash; success triggers a full refresh so any
	 * pulled-in changes light up the tree.
	 */
	async pullChanges() {
		try {
			await ipc.fs.gitPull();
			this.flash('Pull succeeded.');
			void this.refreshActiveFolder();
			return true;
		} catch (err) {
			this.flash(`Pull failed: ${formatError(err)}`);
			return false;
		}
	}

	private async refreshGitStatus(paths: readonly string[], changedSubset: ReadonlySet<string> | null) {
		// Refresh the branch label opportunistically alongside the
		// status fetch — `git symbolic-ref` is cheap and we want the
		// SCM panel header to update when external `git checkout` /
		// `git switch` runs from a terminal. (We can't observe the
		// `.git/HEAD` write directly: the fs-watcher filters `.git/`
		// out to suppress the per-commit storm, so this opportunistic
		// refresh on every status pass is how branch changes
		// propagate back to the UI.)
		void this.refreshGitBranch();
		try {
			this.gitStatusEntries = await ipc.fs.gitStatusEntries([...paths]);
		} catch {
			// Non-fatal — we'd rather leave the tree untinted than
			// noise up the toast for a git probe failure. If git is
			// absent the command still succeeds (returns []), so
			// throwing here is a legitimate filesystem error worth
			// ignoring for tree cosmetics.
			this.gitStatusEntries = [];
		}
		// `HEAD` moves whenever a git status refresh is warranted
		// (commit, checkout, pull, reset). Re-fetch the content
		// backing open text buffers so the gutter flips from
		// "everything's modified" back to clean once the user's
		// commit lands.
		//
		// Same pass also reloads clean buffers from disk: an external
		// mutation (chat agent tool, formatter, integrated terminal)
		// can change the file under us, and the in-memory buffer
		// would otherwise drift from disk silently — the file tree
		// would mark the row `(M)` (because `git status` looks at
		// disk) while the gutter / blame / LSP keep showing the
		// pre-edit content. Dirty buffers are left alone: we don't
		// silently clobber unsaved user edits.
		//
		// `changedSubset` is the path list the watcher actually
		// observed during this debounce window; we narrow the
		// per-buffer reload to the open files that intersect it.
		// `null` means "we don't know which paths moved" (window
		// focus, palette `Refresh File Tree`) — fall back to the
		// conservative loop over every open buffer.
		for (const file of this.openFiles) {
			if (file.kind !== 'text' || file.isDeleted) {
				continue;
			}
			// External buffers don't belong to the active folder's
			// repo or its watcher — neither HEAD refresh nor on-disk
			// reload would route through the right host. They
			// re-read on the next user action (or stay as-is).
			if (file.isExternal) {
				continue;
			}
			if (changedSubset !== null && !changedSubset.has(file.path)) {
				continue;
			}
			this.refreshHead(file.path);
			if (!file.isDirty) {
				void this.reloadOpenFileFromDisk(file.path);
			}
		}
	}

	/**
	 * Re-enumerate the active folder's paths and re-classify git
	 * status against them. A full re-walk is deliberate: a purely-
	 * git-side refresh would miss files the user restored on disk
	 * (`git checkout HEAD -- foo`) because they wouldn't appear in
	 * the tree's `paths` list until `collectPaths` runs again.
	 *
	 * Cheap enough to call on window-focus events and the palette
	 * command — the walk is bounded by `MAX_DEPTH` and
	 * `collectPaths` already runs on every folder open without
	 * complaint. If it ever grows hot enough to matter, the next
	 * step is an fs-watcher (roadmap Phase 5) rather than hand-
	 * written diffing here.
	 *
	 * Collapses with an in-flight walk — firing two focus events
	 * back-to-back (alt-tab flurry) doesn't stack two concurrent
	 * recursions.
	 */
	async refreshActiveFolder(changedSubset: ReadonlySet<string> | null = null, topologyChanged = true): Promise<void> {
		if (!this.activeFolder) {
			return;
		}
		if (this.loadingPaths) {
			return;
		}
		if (topologyChanged) {
			await this.loadPaths(changedSubset);
			return;
		}
		// Modify-only batch: every changed entry already exists in
		// the tree's `paths` snapshot, so the recursive
		// `collect_paths` walk is wasted work. Refresh git status
		// (cheap aggregate IPC) and run the per-buffer loop against
		// the existing path list. The narrowed loop visits only
		// open files in `changedSubset`, so the user's own Ctrl+S
		// becomes one `git status` call and one re-read instead of
		// a tree walk plus N×(git show + read).
		await this.refreshGitStatus(this.paths, changedSubset);
	}

	/** Set to true after `bindFolderChangeRefresh` installs the
	 * fs-watch + focus listeners, so a subsequent HMR-driven
	 * `restoreAppState` doesn't stack duplicate handlers. */
	#folderRefreshWired = false;

	/**
	 * Trailing-debounce timer for `refreshActiveFolder`. The
	 * backend's `notify` watcher fires `fs:changed` once per
	 * filesystem event, and a single user-visible save commonly
	 * produces a small flurry (open/write/close, mtime touches,
	 * directory entry updates, plus our own post-save HEAD content
	 * fetch hitting `.git/`). Without coalescing, each event
	 * triggered a fresh `loadPaths` + `refreshGitStatus` pass; the
	 * latter loops every open buffer through `git show HEAD:` and
	 * `reloadOpenFileFromDisk`, multiplying IPC traffic by N tabs
	 * for what should be one refresh. The trailing debounce holds
	 * 200ms after the last event before firing — enough to absorb
	 * the typical write-burst, short enough that an actual external
	 * mutation (terminal-driven `git checkout`, formatter-by-file)
	 * still feels live.
	 */
	// No frontend debounce: the backend's `notify` watcher already
	// coalesces a single save's burst into one `fs:changed` event,
	// and the topology-aware refresh path is cheap enough that two
	// back-to-back events (rare — a build storm exceeding one
	// 250ms window) doing twice the work doesn't hurt. Earlier
	// revisions trail-debounced here for an extra 100–200ms of
	// padding; once `refreshActiveFolder` learned to skip the
	// recursive `collect_paths` walk for modify-only batches that
	// padding became pure perceived latency on every save with no
	// real benefit. See `fs_watcher.rs#DEBOUNCE` for the backend
	// half.

	/**
	 * Wire the "something changed → refresh the active folder"
	 * hooks. Two independent triggers, each covers a case the
	 * other doesn't:
	 *
	 * - `fs:changed` Tauri event: the backend's `notify` watcher
	 *   fires this on debounced filesystem activity inside the
	 *   active folder. Covers the integrated terminal (where
	 *   window focus never changes) and background processes
	 *   (formatters, build output) changing files while moon-ide
	 *   stays focused.
	 * - Window focus: fires when the user alt-tabs back in. Covers
	 *   the `fs:changed` fallback path — on a folder with too many
	 *   files for inotify, or when the watcher fails to attach,
	 *   this is the only refresh signal we'll get. Also covers
	 *   NFS / SSHFS / Docker-bind-mount scenarios where notify
	 *   can't observe changes at all.
	 *
	 * Both feed `refreshActiveFolder` directly. `fs:changed`
	 * carries the changed paths plus the topology flag so we can
	 * narrow / skip work; the focus path has no payload and falls
	 * back to a conservative full refresh. Concurrent calls
	 * coalesce via the `loadingPaths` guard inside
	 * `refreshActiveFolder` for the topology-true path; for the
	 * cheap modify-only path we let any redundant fires run — the
	 * IPCs are idempotent and the cost is negligible.
	 */
	async bindFolderChangeRefresh(): Promise<void> {
		if (this.#folderRefreshWired) {
			return;
		}
		this.#folderRefreshWired = true;
		try {
			await listen<{ paths: string[]; topologyChanged: boolean }>('fs:changed', ({ payload }) => {
				const subset = payload.paths.length > 0 ? new Set(payload.paths) : null;
				void this.refreshActiveFolder(subset, payload.topologyChanged);
			});
		} catch {
			// Event bus unavailable (tests / non-Tauri). Focus
			// listener still has a shot below.
		}
		try {
			const win = getCurrentWindow();
			await win.onFocusChanged(({ payload: focused }) => {
				if (!focused) {
					return;
				}
				// Focus-driven refresh: no payload, so we don't
				// know what moved. Fall back to the conservative
				// full sweep (null subset + topologyChanged=true).
				void this.refreshActiveFolder();
			});
		} catch {
			// No Tauri window. Palette command is the last
			// resort.
		}
	}

	/**
	 * Subscribe to the LSP broker's two event streams:
	 *
	 * - `lsp:diagnostics` — per-file diagnostic list, full
	 *   replacement. We overwrite the map entry; an empty list
	 *   clears the gutter (server went "you're clean now").
	 * - `lsp:status` — server lifecycle transition. The status bar
	 *   reads these to paint the "install typescript-language-server"
	 *   pill or a "starting…" indicator during the initial tsserver
	 *   boot.
	 *
	 * Idempotent: we only register listeners once per app session,
	 * regardless of how many workspaces open and close. Svelte's
	 * event bus cleans them up when the window shuts down.
	 */
	async bindLspListeners(): Promise<void> {
		if (this.#lspListenersWired) {
			return;
		}
		this.#lspListenersWired = true;
		try {
			await listen<LspDiagnosticsEvent>('lsp:diagnostics', ({ payload }) => {
				const next = new Map(this.diagnostics);
				next.set(payload.path, payload.diagnostics);
				this.diagnostics = next;
			});
			await listen<LspStatusEvent>('lsp:status', ({ payload }) => {
				const next = new Map(this.lspStatuses);
				next.set(payload.languageId, payload);
				this.lspStatuses = next;
			});
		} catch {
			// Event bus unavailable (tests / non-Tauri). The
			// editor will show no diagnostics and the status
			// pill will stay hidden — acceptable degradation.
		}
	}

	/**
	 * Notify the LSP broker that `path` is now open. No-op for file
	 * types without a wired LSP server (see `lspLanguageFor`). The
	 * broker owns everything from here: spawning the server if this
	 * is its first file, sending `textDocument/didOpen`, and
	 * publishing diagnostics back through the event stream.
	 *
	 * Untitled buffers are skipped: they have no on-disk URI, and a
	 * synthetic `untitled:N` URI wouldn't survive a `file://` →
	 * `PathBuf` round-trip on the server side.
	 */
	lspOpen(path: string, text: string) {
		const languageId = lspLanguageFor(path);
		if (!languageId || path.startsWith('untitled:')) {
			return;
		}
		// Swallow failures: if the server crashes mid-session or
		// the backend transiently rejects the call, it's caught by
		// `lsp:status` which surfaces the state in the status bar.
		// A toast on every failed open would be noise — and most
		// "failures" here are the expected graceful degradation
		// (NotAvailable reported with an Ok(())).
		void ipc.lsp.open(path, languageId, text).catch(() => {});
	}

	/**
	 * Debounced `textDocument/didChange`. 150ms matches typical type
	 * cadence without making the server feel sluggish; longer and
	 * diagnostics lag behind what you see on screen, shorter and we
	 * spam the server during bursts (paste, autocomplete accept).
	 */
	lspScheduleUpdate(path: string, text: string) {
		const languageId = lspLanguageFor(path);
		if (!languageId || path.startsWith('untitled:')) {
			return;
		}
		const existing = this.#lspUpdateTimers.get(path);
		if (existing !== undefined) {
			clearTimeout(existing);
		}
		const timer = setTimeout(() => {
			this.#lspUpdateTimers.delete(path);
			void ipc.lsp.update(path, languageId, text).catch(() => {});
		}, 150);
		this.#lspUpdateTimers.set(path, timer);
	}

	/**
	 * Close notification + drop the cached diagnostics for `path`.
	 * The buffer has no more observers in moon-ide, so showing its
	 * stale problem count on next reopen would be wrong.
	 */
	lspClose(path: string) {
		const languageId = lspLanguageFor(path);
		const timer = this.#lspUpdateTimers.get(path);
		if (timer !== undefined) {
			clearTimeout(timer);
			this.#lspUpdateTimers.delete(path);
		}
		if (this.diagnostics.has(path)) {
			const next = new Map(this.diagnostics);
			next.delete(path);
			this.diagnostics = next;
		}
		if (!languageId || path.startsWith('untitled:')) {
			return;
		}
		void ipc.lsp.close(path, languageId).catch(() => {});
	}

	/**
	 * Kick off a blame fetch for `path` and cache the result. Fire-
	 * and-forget: the inline-blame extension binds reactively to
	 * `blameByPath`, so whenever this resolves the widget updates
	 * itself without the caller having to await.
	 *
	 * Skips untitled buffers (no on-disk path to blame) and silently
	 * caches `null` for paths the backend rejects (non-repo,
	 * untracked, binary). Safe to call repeatedly — the backend is
	 * fast for a single file, and we're debounced on the save-refresh
	 * path anyway.
	 */
	refreshBlame(path: string) {
		if (path.startsWith('untitled:')) {
			return;
		}
		if (this.#blameInFlight.has(path)) {
			// Another trigger (first-open + activate, dual-split
			// mount, etc.) already kicked off the fetch. Skipping
			// here avoids a redundant subprocess — the in-flight one
			// will populate the cache for both callers.
			return;
		}
		this.#blameInFlight.add(path);
		void ipc.fs
			.gitBlame(path)
			.then((blame) => {
				// Ignore stale responses: the active folder can have
				// swapped while we were awaiting. `this.openFiles`
				// reflects the current folder, so if the path isn't
				// there any more we drop the answer on the floor.
				if (!this.openFiles.some((f) => f.path === path)) {
					return;
				}
				const next = new Map(this.blameByPath);
				if (blame) {
					next.set(path, blame);
				} else {
					next.delete(path);
				}
				this.blameByPath = next;
			})
			.catch(() => {
				// Non-fatal. No blame = no widget. Backend logs the
				// reason at tracing::debug so `RUST_LOG=moon_core=debug`
				// recovers the detail when triaging.
			})
			.finally(() => {
				this.#blameInFlight.delete(path);
			});
	}

	/**
	 * Debounced version of `refreshBlame` for post-save refreshes.
	 * Save cascades (editorconfig normalisation + user Ctrl+S +
	 * watcher-triggered reloads) can land two or three writes back-
	 * to-back; one `git blame` subprocess at the end of the burst is
	 * enough.
	 */
	scheduleBlameRefresh(path: string) {
		if (path.startsWith('untitled:')) {
			return;
		}
		const existing = this.#blameTimers.get(path);
		if (existing !== undefined) {
			clearTimeout(existing);
		}
		const timer = setTimeout(() => {
			this.#blameTimers.delete(path);
			this.refreshBlame(path);
		}, 250);
		this.#blameTimers.set(path, timer);
	}

	private clearBlameFor(path: string) {
		const timer = this.#blameTimers.get(path);
		if (timer !== undefined) {
			clearTimeout(timer);
			this.#blameTimers.delete(path);
		}
		if (this.blameByPath.has(path)) {
			const next = new Map(this.blameByPath);
			next.delete(path);
			this.blameByPath = next;
		}
	}

	/**
	 * Pull the file's `HEAD` blob into `headByPath` so the editor's
	 * git-changes gutter has something to diff the current buffer
	 * against. Fire-and-forget: the CodeMirror extension binds
	 * reactively to the cache, so whenever this resolves the gutter
	 * redraws.
	 *
	 * Skips untitled buffers (no on-disk path → no `HEAD` blob) and
	 * caches `null` when the backend has nothing to offer (untracked,
	 * not in a repo, binary). The `null` is load-bearing: it prevents
	 * the lazy-fetch guard in `setActive` from re-asking on every
	 * focus, and the extension treats `null` as "no gutter" anyway.
	 */
	refreshHead(path: string) {
		if (path.startsWith('untitled:')) {
			return;
		}
		if (this.#headInFlight.has(path)) {
			return;
		}
		this.#headInFlight.add(path);
		void ipc.fs
			.gitHeadContent(path)
			.then((head) => {
				// Stale-response guard, same reasoning as `refreshBlame`:
				// the active folder can have swapped during the await,
				// in which case this answer is for a file that's no
				// longer open.
				if (!this.openFiles.some((f) => f.path === path)) {
					return;
				}
				const next = new Map(this.headByPath);
				next.set(path, head ?? null);
				this.headByPath = next;
			})
			.catch(() => {
				// Best-effort. No HEAD = empty gutter.
			})
			.finally(() => {
				this.#headInFlight.delete(path);
			});
	}

	private clearHeadFor(path: string) {
		if (this.headByPath.has(path)) {
			const next = new Map(this.headByPath);
			next.delete(path);
			this.headByPath = next;
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
			isDeleted: false,
			isExternal: false,
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
				// Notify the LSP broker only on first open — reopening
				// an already-loaded buffer is a pure UI navigation
				// event, the server still holds its open state. Skip
				// for deleted buffers: there's no on-disk document
				// for the server to track. Blame fetch is handled by
				// `setActive` (called a few lines below) — that path
				// covers session-restored files and cross-folder
				// jumps too, neither of which come through here.
				if (next.kind === 'text' && !next.isDeleted) {
					this.lspOpen(next.path, next.text);
				}
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

	/**
	 * Open a file selected from the native "Open File…" dialog. The
	 * dialog hands us an absolute host path; we route it one of two
	 * ways:
	 *
	 *   - Inside the active folder → fall through to `openFile` on the
	 *     folder-relative path so the buffer gets the full editor
	 *     treatment (LSP, editorconfig, git status / blame / HEAD,
	 *     session persistence).
	 *   - Outside every bound folder → load via `fs.readFileHost`,
	 *     which bypasses every `WorkspaceHost` and reads straight
	 *     from the host filesystem. The buffer enters `openFiles` with
	 *     `isExternal: true`, which disables LSP / editorconfig / git
	 *     wiring and skips session persistence. Saves route through
	 *     `fs.writeFileHost` against the same absolute path. Crucially,
	 *     this stays correct in the Phase 2 container world: the
	 *     in-container host can't see paths outside the bind mount,
	 *     so anything not under the active folder must use the host
	 *     pair.
	 *
	 * Requires an active folder (the open-files list is per-folder).
	 */
	async openHostFile(absolutePath: string, side: SplitSide = this.focusedSide) {
		const folder = this.activeFolder;
		if (!folder) {
			this.flash('Open a folder before opening a file.');
			return;
		}
		const root = folder.path.replace(/\/+$/, '');
		const relative = relativeToRoot(absolutePath, root);
		if (relative !== null) {
			await this.openFile(relative, side);
			return;
		}
		const existing = this.openFiles.find((f) => f.path === absolutePath);
		if (!existing) {
			let result;
			try {
				result = await ipc.fs.readFileHost(absolutePath);
			} catch (err) {
				this.flash(`Failed to open ${absolutePath}: ${formatError(err)}`);
				return;
			}
			if (result.is_binary) {
				this.flash(`Cannot open binary file: ${absolutePath}`);
				return;
			}
			const file: OpenFile = {
				path: absolutePath,
				name: basename(absolutePath),
				kind: 'text',
				isUntitled: false,
				text: result.text,
				previewUrl: '',
				loadedFingerprint: fingerprint(result.text),
				loadedMtimeMs: result.mtime_ms,
				isDirty: false,
				isDeleted: false,
				isExternal: true,
			};
			this.openFiles = [...this.openFiles, file];
		}
		const tabs = this.tabsFor(side);
		if (!tabs.includes(absolutePath)) {
			this.setTabsFor(side, [...tabs, absolutePath]);
		}
		this.setActive(absolutePath, side);
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

	diffModeFor(path: string): boolean {
		return this.diffModes.has(path);
	}

	setDiffMode(path: string, on: boolean) {
		if (this.diffModes.has(path) === on) {
			return;
		}
		const next = new Set(this.diffModes);
		if (on) {
			next.add(path);
		} else {
			next.delete(path);
		}
		this.diffModes = next;
		// Bump focus so the newly-mounted view (Editor or DiffView)
		// gets keyboard focus without an extra click. Toggle UI
		// (button, palette, gutter click) all live outside the
		// editor pane, so they otherwise leave focus on the
		// triggering control.
		this.requestEditorFocus();
	}

	toggleDiffMode(path: string) {
		this.setDiffMode(path, !this.diffModes.has(path));
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
		// External buffers live outside every bound folder; the active
		// folder's `.editorconfig` cascade is the wrong tree to walk.
		// Defaults are fine here — the team rarely edits unrelated
		// host files long enough for indent settings to matter.
		const file = this.openFiles.find((f) => f.path === path);
		if (file?.isExternal) {
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
		// Missing-on-disk is the expected shape for a working-tree
		// deletion that the user still wants to read (`git rm`'d or
		// just deleted from the tree). We fall through to a HEAD
		// read before surfacing the error, so a click on a deleted
		// row in the tree — and a session-restored tab whose file
		// vanished between launches — both land in diff view
		// instead of flashing "Failed to open".
		let result;
		try {
			result = await ipc.fs.readFile(path);
		} catch (err) {
			const deleted = await this.loadDeletedFile(path);
			if (deleted) {
				return deleted;
			}
			throw err;
		}
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
			isDeleted: false,
			isExternal: false,
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
			isDeleted: false,
			isExternal: false,
		};
	}

	/**
	 * Build an `OpenFile` for a path whose working-tree copy is gone
	 * but that still has a `HEAD` revision. `text` holds the `HEAD`
	 * content so the diff view has a stable "before" side without a
	 * second fetch.
	 *
	 * Returns `null` silently when there's no `HEAD` content (the
	 * path isn't in a repo, or was never tracked). Callers fall
	 * back to their own error reporting — the most common caller is
	 * `loadTextFile`'s deleted-file recovery path, where "no HEAD
	 * either" means the original disk-read error is the right
	 * message to surface.
	 */
	private async loadDeletedFile(path: string): Promise<OpenFile | null> {
		const headText = await ipc.fs.gitHeadContent(path);
		if (headText === null) {
			return null;
		}
		return {
			path,
			name: basename(path),
			kind: 'text',
			isUntitled: false,
			text: headText,
			previewUrl: '',
			loadedFingerprint: fingerprint(headText),
			loadedMtimeMs: null,
			isDirty: false,
			isDeleted: true,
			isExternal: false,
		};
	}

	/**
	 * Create an empty file at `path` (workspace-relative). Surfaces
	 * backend errors via `flash` rather than swallowing them — the
	 * file-tree's New File flow opens an inline-rename input and
	 * the user expects feedback when their chosen name collides
	 * with an existing file or hits a permission boundary. On
	 * success, the fs-watcher's `fs:changed` event triggers the
	 * usual tree refresh and the new path lights up; we also open
	 * the file so the next keystroke goes into the right buffer.
	 */
	async createFile(path: string) {
		try {
			await ipc.fs.createFile(path);
			await this.openFile(path);
		} catch (err) {
			this.flash(`Create file failed: ${formatError(err)}`);
		}
	}

	/**
	 * Create an empty directory at `path` (workspace-relative).
	 * Same error-surfacing semantics as `createFile`. We don't open
	 * the directory in the editor — directories aren't opened, only
	 * folder-bar-bound. The post-create tree refresh reveals it in
	 * the file tree.
	 */
	async createDir(path: string) {
		try {
			await ipc.fs.createDir(path);
		} catch (err) {
			this.flash(`Create folder failed: ${formatError(err)}`);
		}
	}

	/**
	 * Rename a file or directory from `from` to `to` (both
	 * workspace-relative). Open buffers whose path matched `from`
	 * — or, for a directory rename, lived inside `from` — get
	 * their path field rewritten in place so the editor doesn't
	 * lose the buffer. The fs-watcher's refresh propagates the
	 * rename into the tree and git status. Errors surface as a
	 * toast; the user can retry from the inline input or use a
	 * manual move.
	 */
	async renamePath(from: string, to: string) {
		if (from === to) {
			return;
		}
		try {
			await ipc.fs.rename(from, to);
		} catch (err) {
			this.flash(`Rename failed: ${formatError(err)}`);
			return;
		}
		const fromIsDir = from.endsWith('/');
		const fromPrefix = fromIsDir ? from : `${from}/`;
		const toPrefix = to.endsWith('/') ? to : `${to}/`;
		const remap = (p: string): string => {
			if (p === from) {
				return to;
			}
			if (p.startsWith(fromPrefix)) {
				return toPrefix + p.slice(fromPrefix.length);
			}
			return p;
		};
		const renamedPaths: { from: string; to: string }[] = [];
		this.openFiles = this.openFiles.map((f) => {
			const newPath = remap(f.path);
			if (newPath === f.path) {
				return f;
			}
			renamedPaths.push({ from: f.path, to: newPath });
			return { ...f, path: newPath, name: basename(newPath) };
		});
		this.leftTabs = this.leftTabs.map(remap);
		this.rightTabs = this.rightTabs.map(remap);
		if (this.leftActive !== null) {
			this.leftActive = remap(this.leftActive);
		}
		if (this.rightActive !== null) {
			this.rightActive = remap(this.rightActive);
		}
		// Stamp the most-recently-renamed file/folder pair so the
		// Editor's `isRename` check can tell "this path swap is a
		// rename" from "this path swap is a tab switch". For a
		// directory rename that touched several open buffers we
		// pick the active one (or the last one in iteration order)
		// — the Editor only consults `isRename` for whichever
		// buffer is currently mounted, and a multi-file rename
		// always resolves through this code path before any
		// individual editor effect runs.
		const stamp =
			renamedPaths.find((p) => p.to === this.leftActive || p.to === this.rightActive) ??
			renamedPaths[renamedPaths.length - 1] ??
			null;
		if (stamp !== null) {
			this.lastRename = stamp;
			this.renameTick += 1;
		}
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
	 * Discard the user's local changes to `paths`. Routing depends on
	 * each path's git status:
	 *
	 * - `modified` / `deleted` → `git restore --source=HEAD --staged
	 *   --worktree` brings the file back to its committed content (or
	 *   un-deletes it).
	 * - `untracked` → sent to the OS trash. `git restore` has nothing
	 *   to restore them *to*; the only "undo the change" move is to
	 *   make the file go away. Reversible from the OS trash bin.
	 * - `added` / `ignored` → silently dropped. Added files are a
	 *   conscious `git add` choice whose correct reversal ("unstage"
	 *   vs "delete") is ambiguous; the menu omits this action for
	 *   them. Ignored files aren't a "change" in any meaningful
	 *   sense.
	 *
	 * The confirm dialog always fires: `git restore` is irreversible,
	 * and trashing an untracked file is only reversible via the OS
	 * UI. Dirty open buffers for restored paths are discarded without
	 * a per-tab prompt for the same reason `removePaths` skips them —
	 * the user just confirmed they want changes gone.
	 */
	async discardPaths(paths: readonly string[]) {
		if (paths.length === 0) {
			return;
		}
		const statusMap = new Map(this.gitStatusEntries.map((e) => [e.path, e.status]));
		const toRestore: string[] = [];
		const toTrash: string[] = [];
		for (const path of paths) {
			const status = statusMap.get(path);
			if (status === 'modified' || status === 'deleted') {
				toRestore.push(path);
			} else if (status === 'untracked') {
				toTrash.push(path);
			}
		}
		const total = toRestore.length + toTrash.length;
		if (total === 0) {
			return;
		}

		// Skip confirm when the user is doing a pure un-delete:
		// every target is a `deleted` path being restored from HEAD.
		// Nothing is thrown away, nothing lands in the trash, and
		// git already holds the exact content we're bringing back —
		// the dialog adds friction without adding safety. Any other
		// mix (modified, untracked) still fires it: modified means
		// we're discarding live edits, untracked means a trash trip
		// with no git recovery path.
		const isPureUndelete = toTrash.length === 0 && toRestore.every((p) => statusMap.get(p) === 'deleted');
		if (!isPureUndelete) {
			const message = buildDiscardMessage(toRestore, toTrash);
			const ok = await confirm(message, {
				title: 'Discard changes',
				okLabel: 'Discard',
				cancelLabel: 'Cancel',
			});
			if (!ok) {
				return;
			}
		}

		// Order matters only in so far as the folder refresh we fire
		// at the end should see the final state on disk. Both calls
		// are independent git/fs operations; fire them in parallel.
		const restorePromise =
			toRestore.length > 0
				? ipc.fs.gitRestorePaths(toRestore).then(
						() => ({ kind: 'restore' as const, ok: true }),
						(err: unknown) => ({ kind: 'restore' as const, ok: false, err }),
					)
				: Promise.resolve({ kind: 'restore' as const, ok: true });
		const trashSettled =
			toTrash.length > 0 ? Promise.allSettled(toTrash.map((p) => ipc.fs.trash(p))) : Promise.resolve([]);
		const [restoreResult, trashResults] = await Promise.all([restorePromise, trashSettled]);

		const failures: { path: string; err: unknown }[] = [];
		if (!restoreResult.ok) {
			// `git restore` is batched — we get one verdict for the
			// whole set. Attribute the failure to the first path so
			// the toast has something concrete to name.
			const first = toRestore[0] ?? 'unknown';
			failures.push({ path: first, err: 'err' in restoreResult ? restoreResult.err : 'unknown' });
		}
		const trashedOk: string[] = [];
		trashResults.forEach((r, i) => {
			const p = toTrash[i] ?? '';
			if (r.status === 'fulfilled') {
				trashedOk.push(p);
			} else {
				failures.push({ path: p, err: r.reason });
			}
		});

		// Reload open buffers for restored paths so the editor
		// reflects the committed content rather than the (now
		// discarded) working-tree text.
		if (restoreResult.ok && toRestore.length > 0) {
			const restoredSet = new Set(toRestore);
			await Promise.all(
				this.openFiles
					.filter((f) => f.kind === 'text' && restoredSet.has(f.path))
					.map((f) => this.reloadOpenFileFromDisk(f.path)),
			);
		}

		// Close any tabs for untracked files we just trashed (same
		// logic `removePaths` applies — the file no longer exists, so
		// the tab is pointing at nothing).
		if (trashedOk.length > 0) {
			const removedSet = new Set(trashedOk);
			this.openFiles = this.openFiles.filter((f) => !removedSet.has(f.path));
			this.leftTabs = this.leftTabs.filter((p) => !removedSet.has(p));
			this.rightTabs = this.rightTabs.filter((p) => !removedSet.has(p));
			if (this.leftActive !== null && removedSet.has(this.leftActive)) {
				this.leftActive = this.leftTabs[this.leftTabs.length - 1] ?? null;
			}
			if (this.rightActive !== null && removedSet.has(this.rightActive)) {
				this.rightActive = this.rightTabs[this.rightTabs.length - 1] ?? null;
			}
		}

		if (failures.length > 0) {
			const first = failures[0];
			const reason = first ? formatError(first.err) : 'unknown error';
			this.flash(`Discard failed for ${failures.length} of ${total}: ${reason}`);
		}

		await this.loadPaths();
		this.persistAppState();
	}

	/**
	 * Pull the on-disk text for `path` into the matching open buffer.
	 * Used by `discardPaths` after a `git restore` and by
	 * `refreshGitStatus` to sync clean buffers with external
	 * mutations (chat agent tools, formatters, terminal commands).
	 * Silently no-ops if the buffer was closed between the call
	 * kicking off and this resolving, or if the on-disk text already
	 * matches the live buffer (avoids reactive churn on every
	 * `fs:changed` event for files that *we* just wrote).
	 */
	private async reloadOpenFileFromDisk(path: string) {
		const next = await this.loadTextFile(path);
		if (!next) {
			return;
		}
		const current = this.openFiles.find((f) => f.path === path);
		if (current && current.kind === 'text' && next.kind === 'text' && current.text === next.text) {
			return;
		}
		this.openFiles = this.openFiles.map((f) => (f.path === path ? next : f));
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

		// Skip the confirm dialog when every target is safely
		// recoverable via git — i.e. tracked-clean files, or folders
		// whose descendants are all tracked-clean. The "are you
		// sure?" dialog exists as a safety net against accidental
		// keypresses; for paths git can bring back unchanged in one
		// command the net is net annoyance, not protection. Dirty
		// paths, untracked paths, and anything outside a git repo
		// still fire the dialog — there, an accidental delete _is_
		// a data-loss event.
		if (total > 0 && !canSkipRemovalConfirm(real, this.gitStatusEntries)) {
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
		let diffsChanged = false;
		const diffs = new Set(this.diffModes);
		for (const key of diffs) {
			if (wasRemoved(key)) {
				diffs.delete(key);
				diffsChanged = true;
			}
		}
		if (diffsChanged) {
			this.diffModes = diffs;
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
			// External buffers were never opened with the LSP / git
			// machinery, so capture the flag before the filter and
			// skip the matching teardown. Calling `lspClose` here
			// would issue a `didClose` for a `didOpen` the broker
			// never saw.
			const wasExternal = this.openFiles.find((f) => f.path === path)?.isExternal === true;
			this.openFiles = this.openFiles.filter((f) => f.path !== path);
			if (this.previewModes.has(path)) {
				const next = new Map(this.previewModes);
				next.delete(path);
				this.previewModes = next;
			}
			if (this.diffModes.has(path)) {
				const next = new Set(this.diffModes);
				next.delete(path);
				this.diffModes = next;
			}
			if (!wasExternal) {
				// Tell the LSP broker *and* drop any cached diagnostics
				// so a later reopen of the same path starts clean rather
				// than flashing stale squigglies.
				this.lspClose(path);
				// Drop blame cache + any pending refresh timer. If the
				// user reopens the file later a fresh blame fetch runs;
				// no point keeping the old one warm.
				this.clearBlameFor(path);
				this.clearHeadFor(path);
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
		// Lazy-seed blame for the newly-active buffer. Covers all
		// paths into `openFiles`: fresh `openFile`, session restore
		// (which bulk-populates the list without calling openFile),
		// cross-folder go-to-definition, and so on. The in-flight
		// guard in `refreshBlame` keeps this cheap — once the cache
		// has an entry or a fetch is pending, this call is a no-op.
		const file = this.openFiles.find((f) => f.path === path);
		// External buffers live outside every bound folder, so the
		// active folder's git layer has nothing to say about them
		// (no HEAD, no blame). Skip both seeds — they'd just round-
		// trip to the host for a guaranteed-empty answer.
		const isLiveTextBuffer = file && file.kind === 'text' && !file.isDeleted && !file.isExternal;
		if (isLiveTextBuffer && !this.blameByPath.has(path)) {
			this.refreshBlame(path);
		}
		// HEAD cache feeds both the editor's git-change gutter and
		// the DiffView's "before" side, so a single seed warms both
		// surfaces. `isDeleted` buffers stash HEAD in `text` directly
		// (see `loadDeletedFile`) and don't need a second fetch.
		if (isLiveTextBuffer && !this.headByPath.has(path)) {
			this.refreshHead(path);
		}
		// Nav history: only push on genuine user navigation. We're
		// inside `navigateBack` / `navigateForward` / `jumpTo` when
		// `suppressNavPush` is set; skipping the push there prevents
		// the immediate-rewind loop ("back arrow pushes the previous
		// page onto history, so the next back arrow comes back here
		// forever").
		if (!this.suppressNavPush) {
			this.pushFileSwitchEntry(path);
		}
		// `focus` defaults to true; the tree opts out via `{ focus: false }`
		// so arrow-key navigation can preview-browse without stealing focus.
		if (options.focus !== false) {
			this.requestEditorFocus();
		}
		this.persistAppState();
	}

	/**
	 * Record a file-switch. Captures the caret from the previous tip
	 * (so forward navigation lands where the user actually was, not
	 * where a tool last parked the caret) and pushes a fresh entry
	 * for `path` at line 0 by default. The Editor's first selection
	 * update after the file opens will correct the entry to the real
	 * caret position.
	 */
	private pushFileSwitchEntry(path: string) {
		const folder = this.activeFolderPath;
		if (folder === null) {
			return;
		}
		const tip = this.navIndex >= 0 ? this.navStack[this.navIndex] : undefined;
		if (tip && tip.folder === folder && tip.path === path) {
			// Already the tip — arrow-key selection updates maintain
			// caret, no need to push.
			return;
		}
		// Truncate forward and append. Initial caret is (0, 0); Editor
		// will refine via `updateNavTip` as soon as its selection
		// settles (or the pendingJumps consumer lands a specific
		// position).
		const trimmed = this.navStack.slice(0, this.navIndex + 1);
		trimmed.push({ folder, path, line: 0, character: 0 });
		this.navStack = trimmed;
		this.navIndex = trimmed.length - 1;
	}

	/**
	 * Record a mouse click / explicit jump inside a file. Unlike
	 * `pushFileSwitchEntry`, this always pushes a fresh entry even
	 * when the path is already the tip — the whole point of position
	 * history is that clicking at line 50 while you were reading
	 * line 10 leaves a bookmark at 10 you can Alt+Left back to.
	 *
	 * The only suppression is the no-move case: a click that lands on
	 * the exact same caret position as the tip is a focus/refocus
	 * gesture, not a navigation. Record every real caret move.
	 */
	pushClickNavigation(folder: string, path: string, position: { line: number; character: number }) {
		if (this.suppressNavPush) {
			return;
		}
		const tip = this.navIndex >= 0 ? this.navStack[this.navIndex] : undefined;
		if (
			tip &&
			tip.folder === folder &&
			tip.path === path &&
			tip.line === position.line &&
			tip.character === position.character
		) {
			return;
		}
		const trimmed = this.navStack.slice(0, this.navIndex + 1);
		trimmed.push({ folder, path, line: position.line, character: position.character });
		this.navStack = trimmed;
		this.navIndex = trimmed.length - 1;
	}

	/**
	 * Selection moved without a genuine "navigation" gesture (arrow
	 * keys, selection extension, programmatic setSelection). Mutate
	 * the tip so Alt+Right after a back-nav restores the caret where
	 * the user actually left it, not where the original click landed.
	 * No-op if the tip doesn't match (editor is rendering some other
	 * file, which shouldn't happen but is cheap to guard).
	 */
	updateNavTip(folder: string, path: string, position: { line: number; character: number }) {
		if (this.suppressNavPush) {
			return;
		}
		const tip = this.navIndex >= 0 ? this.navStack[this.navIndex] : undefined;
		if (!tip || tip.folder !== folder || tip.path !== path) {
			return;
		}
		tip.line = position.line;
		tip.character = position.character;
	}

	/** True when Alt+Left has somewhere to go. */
	canNavigateBack = $derived(this.navIndex > 0);
	/** True when Alt+Right has somewhere to go. */
	canNavigateForward = $derived(this.navIndex >= 0 && this.navIndex < this.navStack.length - 1);

	/**
	 * Step back one entry in nav history. Switches folder first if
	 * the target entry belongs to a different bound folder, then
	 * opens the file and restores the stored caret. No-op when we're
	 * already at the oldest entry — the keybinding falls through to
	 * CM's default (word-motion on mac, nothing on win/linux) via the
	 * return value.
	 */
	async navigateBack(): Promise<boolean> {
		if (!this.canNavigateBack) {
			return false;
		}
		this.navIndex -= 1;
		const entry = this.navStack[this.navIndex];
		if (!entry) {
			return false;
		}
		await this.restoreNavEntry(entry);
		return true;
	}

	async navigateForward(): Promise<boolean> {
		if (!this.canNavigateForward) {
			return false;
		}
		this.navIndex += 1;
		const entry = this.navStack[this.navIndex];
		if (!entry) {
			return false;
		}
		await this.restoreNavEntry(entry);
		return true;
	}

	/**
	 * Open the file referenced by `entry` and restore its caret.
	 * Handles the cross-folder case: if `entry.folder` differs from
	 * the active folder, we swap active folders first (which
	 * rehydrates its buffers / tree / terminal), then open the file.
	 * `suppressNavPush` is held across the whole transaction so the
	 * setActiveFolder + openFile chain doesn't push duplicate entries
	 * as the folder-switch drags the caret around.
	 *
	 * Bails (with a flash) if the target folder has been removed from
	 * the workspace since the entry was recorded — nav-history
	 * cleanup on `removeFolder` prunes most of these, but a stale
	 * forward-stack slice can outlive the prune, so we belt-and-brace.
	 */
	private async restoreNavEntry(entry: NavEntry): Promise<void> {
		const folderExists = this.workspace?.folders.some((f) => f.path === entry.folder) ?? false;
		if (!folderExists) {
			this.flash(`Folder no longer in workspace: ${entry.folder}`);
			return;
		}
		const key = navKey(entry.folder, entry.path);
		const next = new SvelteMap(this.pendingJumps);
		next.set(key, { line: entry.line, character: entry.character });
		this.pendingJumps = next;
		this.suppressNavPush = true;
		try {
			if (this.activeFolderPath !== entry.folder) {
				await this.setActiveFolder(entry.folder);
			}
			await this.openFile(entry.path);
		} finally {
			this.suppressNavPush = false;
		}
	}

	/**
	 * Drop any nav-stack entries pointing at a folder that no longer
	 * lives in the workspace. Called from `removeFolder` so Alt+Left
	 * doesn't try to navigate into a folder the user just kicked out.
	 * Adjusts `navIndex` to keep pointing at the relatively-equivalent
	 * current position (or -1 if every entry got pruned).
	 */
	private pruneNavEntriesForFolder(folderPath: string) {
		if (this.navStack.length === 0) {
			return;
		}
		const current = this.navIndex >= 0 ? this.navStack[this.navIndex] : undefined;
		const filtered = this.navStack.filter((entry) => entry.folder !== folderPath);
		this.navStack = filtered;
		if (!current || current.folder === folderPath) {
			this.navIndex = filtered.length - 1;
		} else {
			this.navIndex = filtered.indexOf(current);
		}
	}

	/**
	 * Open `path` and land the caret at `position`. Used by
	 * Ctrl/Cmd-click go-to-definition: stashes the position in
	 * `pendingJumps` so the Editor's reactive effect dispatches a
	 * selection-change after the state-rebuild settles.
	 *
	 * `folder` defaults to the active folder. Pass an explicit folder
	 * for cross-folder jumps (e.g. goto-definition resolving into a
	 * different bound folder); the method switches folders first.
	 * Records a normal nav entry — back/forward works across
	 * definition jumps and carries the exact caret.
	 */
	async jumpTo(
		path: string,
		position: { line: number; character: number },
		side: SplitSide = this.focusedSide,
		folder: string = this.activeFolderPath ?? '',
	) {
		if (!folder) {
			return;
		}
		const key = navKey(folder, path);
		const next = new SvelteMap(this.pendingJumps);
		next.set(key, position);
		this.pendingJumps = next;
		if (this.activeFolderPath !== folder) {
			await this.setActiveFolder(folder);
		}
		await this.openFile(path, side);
		// Record the arrival position explicitly. setActive already
		// pushed a file-switch entry at (0, 0); overwrite it with the
		// real target so Alt+Left from here jumps to *this* spot, not
		// the top of the file.
		const tip = this.navIndex >= 0 ? this.navStack[this.navIndex] : undefined;
		if (tip && tip.folder === folder && tip.path === path) {
			tip.line = position.line;
			tip.character = position.character;
		}
	}

	/**
	 * Resolve an LSP-returned external URI (the active-folder broker
	 * marks everything outside its root as "external") against every
	 * bound folder in the workspace. Returns `{ folder, path }` when
	 * the URI falls under some bound folder so a cross-folder jump
	 * can happen; `null` means it's genuinely outside the workspace
	 * (node_modules / toolchain / http-scheme).
	 *
	 * Only accepts `file://` URIs — anything else (e.g. `ts://` pseudo
	 * URIs for TS built-ins) is inherently outside the workspace.
	 * Matches longest folder-prefix first so nested folder bindings
	 * (root + `root/subcrate`) resolve against the inner folder.
	 */
	resolveExternalUri(externalUri: string): { folder: string; path: string } | null {
		if (!externalUri.startsWith('file://')) {
			return null;
		}
		let abs: string;
		try {
			abs = decodeURIComponent(new URL(externalUri).pathname);
		} catch {
			return null;
		}
		const ws = this.workspace;
		if (!ws) {
			return null;
		}
		// Sort by descending length so `root/sub` beats `root` when
		// both are bound — otherwise a file inside the nested folder
		// would resolve against the outer one.
		const sorted = ws.folders.toSorted((a, b) => b.path.length - a.path.length);
		for (const folder of sorted) {
			const root = folder.path.endsWith('/') ? folder.path : `${folder.path}/`;
			if (abs === folder.path) {
				return { folder: folder.path, path: '' };
			}
			if (abs.startsWith(root)) {
				return { folder: folder.path, path: abs.slice(root.length) };
			}
		}
		return null;
	}

	/**
	 * Called by the Editor once it's applied a pending jump for
	 * `(folder, path)`. The entry is one-shot — next paint shouldn't
	 * re-jump the caret if the user moved it away.
	 */
	consumePendingJump(folder: string, path: string) {
		const key = navKey(folder, path);
		if (!this.pendingJumps.has(key)) {
			return;
		}
		const next = new SvelteMap(this.pendingJumps);
		next.delete(key);
		this.pendingJumps = next;
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

	/** Focus the editor and run local autocomplete (direct apply; same as Ctrl+T). */
	requestAutocomplete() {
		this.requestEditorFocus();
		this.autocompleteEditorTick += 1;
	}

	beginAutocompleteRequest() {
		this.autocompleteInFlight = true;
	}

	endAutocompleteRequest() {
		this.autocompleteInFlight = false;
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
			let diffs: Set<string> | null = null;
			for (const path of dropped) {
				if (this.diffModes.has(path)) {
					if (!diffs) {
						diffs = new Set(this.diffModes);
					}
					diffs.delete(path);
				}
			}
			if (diffs) {
				this.diffModes = diffs;
			}
		}
		this.persistAppState();
	}

	updateText(path: string, text: string) {
		let isExternal = false;
		this.openFiles = this.openFiles.map((f) => {
			if (f.path !== path) {
				return f;
			}
			isExternal = f.isExternal;
			// Length mismatch is a fast path: different sizes can never compare
			// equal, so we skip hashing entirely while the user is typing.
			const dirty =
				text.length !== f.loadedFingerprint.length || !fingerprintEquals(fingerprint(text), f.loadedFingerprint);
			return { ...f, text, isDirty: dirty };
		});
		// Debounced didChange: one IPC per burst of keystrokes. The
		// backend does the serialisation with the LSP server, so a
		// dropped update (e.g. closeFile racing) can never leave us
		// in a state that disagrees with the server's last version.
		// External buffers never had a `didOpen` (no folder-rooted LSP
		// to send it to), so skip didChange too.
		if (!isExternal) {
			this.lspScheduleUpdate(path, text);
		}
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
		// Clean buffers used to short-circuit here, but with the
		// format-on-save pipeline (specs/decisions/0012) the host's
		// `save_file` can rewrite even unchanged bytes — the user's
		// expected mental model is "Ctrl+S formats my file regardless
		// of whether I just typed something". The cost is one IPC
		// roundtrip per save; cheap and the pre-save pipeline /
		// formatter / re-read are all idempotent on already-canonical
		// content (the writes that come back equal don't bump dirty
		// state, the LSP didChange is a no-op, the fingerprint stays
		// stable).
		try {
			// External buffers route through the host-direct write so
			// the bytes land on the host filesystem regardless of
			// whether the active folder is currently containerised.
			// They also skip the editorconfig + lint-staged save
			// pipeline (no workspace root to anchor the cascade
			// against) and the post-save blame / head refresh
			// (external paths aren't in the active folder's repo).
			if (file.isExternal) {
				const result = await ipc.fs.writeFileHost(file.path, file.text);
				this.openFiles = this.openFiles.map((f) =>
					f.path === file.path
						? {
								...f,
								isDirty: false,
								loadedFingerprint: fingerprint(f.text),
								loadedMtimeMs: result.mtime_ms,
							}
						: f,
				);
				return;
			}
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
			// A fresh commit or amended working tree changes what
			// `git blame` reports for most lines in this file. The
			// in-memory buffer won't magically re-attribute itself
			// — kick a debounced refresh so the widget catches up.
			this.scheduleBlameRefresh(file.path);
			// The git-changes gutter doesn't need a save-time refresh:
			// `HEAD` doesn't move when we write the working tree, and
			// the buffer's own reactive update already re-diffs on
			// the new text. Refresh is driven by `refreshGitStatus`
			// instead (which *does* fire after external `git commit`
			// / `checkout` via filesystem watcher events).
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
		const folder = this.activeFolder;
		if (!folder) {
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
			? `${folder.path}/${file.name}.txt`
			: await ipc.fs.absolutePath(file.path).catch(() => `${folder.path}/${file.path}`);
		const target = await saveDialog({
			title: file.isUntitled ? 'Save Untitled File' : 'Save As',
			defaultPath,
		});
		if (!target) {
			return;
		}

		// Folder-bound: the host-side fs commands all take folder-
		// relative paths against the active folder's host, and our
		// session model assumes every open file lives inside the
		// folder it was opened from. Refuse out-of-folder saves with
		// a toast — those would conceptually want to land in a
		// different folder of the workspace, and we don't have a
		// "move buffer to folder X" UX.
		const root = folder.path.replace(/\/+$/, '');
		const newPath = relativeToRoot(target, root);
		if (newPath === null) {
			this.flash('Save target must be inside the active folder.');
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
				if (this.diffModes.has(oldPath)) {
					const next = new Set(this.diffModes);
					next.delete(oldPath);
					next.add(newPath);
					this.diffModes = next;
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

	/**
	 * Persist `mode` as the user's theme choice and repaint. Same
	 * shape `toggleTheme` used to take, just for a three-valued
	 * enum — picking `'system'` defers the resolved dark/light
	 * flip to [`effectiveTheme`], which also reacts to OS theme
	 * changes without a save.
	 */
	setTheme(mode: ThemeMode) {
		if (this.theme === mode) {
			return;
		}
		this.theme = mode;
		applyTheme(this.effectiveTheme);
		this.persistAppState();
	}

	/** URL used for `GET /health` and `/completion`: external override, or derived from listen host/port. */
	nextEditEffectiveHttpBase(): string {
		const ext = this.nextEditExternalBaseUrl.trim();
		if (ext.length > 0) {
			return ext;
		}
		const h = this.nextEditServerHost.trim() || '127.0.0.1';
		const p = this.nextEditServerPort;
		if (p >= 1 && p <= 65535) {
			return `http://${h}:${p}`;
		}
		return DEFAULT_NEXT_EDIT_BASE_URL;
	}

	setNextEditExternalBaseUrl(url: string) {
		this.nextEditExternalBaseUrl = url.trim();
		if (this.nextEditExternalBaseUrl.length > 0) {
			this.nextEditServerAutostart = false;
		}
		this.persistAppState();
		void this.refreshNextEditProbe();
	}

	setNextEditLlamaBinary(s: string) {
		this.nextEditLlamaBinary = s;
		this.persistAppState();
	}

	setNextEditHfRepo(s: string) {
		this.nextEditHfRepo = s;
		this.persistAppState();
	}

	setNextEditServerHost(s: string) {
		this.nextEditServerHost = s.trim().length > 0 ? s.trim() : '127.0.0.1';
		this.persistAppState();
		void this.refreshNextEditProbe();
	}

	setNextEditServerPort(port: number) {
		if (!Number.isFinite(port) || port < 1 || port > 65535) {
			return;
		}
		this.nextEditServerPort = Math.floor(port);
		this.persistAppState();
		void this.refreshNextEditProbe();
	}

	async refreshNextEditServerStatus() {
		try {
			this.nextEditServerSnapshot = await ipc.nextEdit.serverStatus();
		} catch {
			this.nextEditServerSnapshot = null;
		}
	}

	private async refreshNextEditServerStatusThenMaybeAutostart() {
		await this.refreshNextEditServerStatus();
		this.maybeAutostartNextEditServer();
	}

	/** After hydrate: start managed llama-server when the user left it enabled (non-external URL only). */
	private maybeAutostartNextEditServer() {
		if (!this.nextEditServerAutostart) {
			return;
		}
		if (this.nextEditExternalBaseUrl.trim().length > 0) {
			return;
		}
		if (!this.nextEditHfRepo.trim()) {
			return;
		}
		if (this.nextEditServerSnapshot?.running) {
			return;
		}
		void this.startNextEditServer();
	}

	async startNextEditServer() {
		if (this.nextEditServerActionInFlight) {
			return;
		}
		if (this.nextEditExternalBaseUrl.trim().length > 0) {
			this.flash('Clear the external server URL (advanced) to start llama-server from moon-ide.');
			return;
		}
		this.nextEditServerAutostart = true;
		this.persistAppState();
		this.nextEditServerActionInFlight = true;
		try {
			const snap = await ipc.nextEdit.serverStart({
				llamaBinary: this.nextEditLlamaBinary.trim(),
				hfRepo: this.nextEditHfRepo.trim(),
				serverHost: this.nextEditServerHost.trim() || '127.0.0.1',
				serverPort: this.nextEditServerPort,
			});
			this.nextEditServerSnapshot = snap;
			if (snap.startError) {
				this.flash(`llama-server: ${snap.startError}`);
			}
			void this.refreshNextEditProbe();
		} catch (e) {
			this.flash(`Could not start llama-server: ${formatError(e)}`);
			void this.refreshNextEditServerStatus();
		} finally {
			this.nextEditServerActionInFlight = false;
		}
	}

	async stopNextEditServer() {
		if (this.nextEditServerActionInFlight) {
			return;
		}
		this.nextEditServerAutostart = false;
		this.persistAppState();
		this.nextEditServerActionInFlight = true;
		try {
			this.nextEditServerSnapshot = await ipc.nextEdit.serverStop();
			void this.refreshNextEditProbe();
		} catch (e) {
			this.flash(`Could not stop llama-server: ${formatError(e)}`);
		} finally {
			this.nextEditServerActionInFlight = false;
		}
	}

	/** Probes `GET {base}/health` on the llama.cpp server. Best-effort. */
	async refreshNextEditProbe() {
		if (this.nextEditProbeInFlight) {
			return;
		}
		this.nextEditProbeInFlight = true;
		try {
			const base = this.nextEditEffectiveHttpBase().trim();
			if (base.length === 0) {
				this.nextEditProbe = { kind: 'error', detail: 'Effective URL is empty' };
				return;
			}
			this.nextEditProbe = await ipc.nextEdit.probe(base);
		} catch (e) {
			this.nextEditProbe = { kind: 'unreachable', detail: formatError(e) };
		} finally {
			this.nextEditProbeInFlight = false;
		}
	}

	/**
	 * Probe the OS colour-scheme preference and subscribe to further
	 * changes. Safe to call multiple times — guarded by
	 * `systemPreferenceBound`.
	 *
	 * Primary source is the `system_theme` Tauri command, which on
	 * Linux walks the XDG Desktop Portal `color-scheme` D-Bus setting
	 * (same channel modern browsers use). That detour is load-bearing:
	 * WebKitGTK's own `matchMedia('(prefers-color-scheme: dark)')`
	 * answer and `getCurrentWindow().theme()` _both_ ignore the GTK
	 * / GNOME / KDE theme and default to light, so using either as
	 * the truth flips the UI to light on startup for every user on a
	 * Linux dark desktop.
	 *
	 * `getCurrentWindow().onThemeChanged` covers macOS / Windows
	 * runtime flips (where the webview _does_ track the OS); on
	 * Linux the event rarely fires but harmlessly double-pings a
	 * value we already re-read on every poll. matchMedia is left in
	 * purely as a fallback for non-Tauri dev shells (vite-only).
	 *
	 * Not bound on construction because the Tauri runtime isn't
	 * necessarily up yet at module-eval; `restoreAppState` awaits
	 * this before touching `applyTheme`.
	 */
	private async bindSystemPreference() {
		if (this.systemPreferenceBound) {
			return;
		}
		this.systemPreferenceBound = true;

		if (typeof window !== 'undefined' && typeof window.matchMedia === 'function') {
			const mq = window.matchMedia('(prefers-color-scheme: dark)');
			mq.addEventListener('change', (event) => {
				this.updateSystemPrefersDark(event.matches);
			});
		}

		try {
			const theme = await ipc.system.theme();
			this.updateSystemPrefersDark(resolveSystemTheme(theme));
		} catch {
			// Backend probe unavailable (tests, vite-only shell, or the
			// XDG portal is genuinely not reachable). Fall back to
			// whatever matchMedia already seeded.
		}

		try {
			// Linux runtime flips arrive via this event (the desktop
			// shell's ashpd watcher re-emits the XDG portal signal).
			await listen<SystemTheme>('system:theme-changed', ({ payload }) => {
				this.updateSystemPrefersDark(resolveSystemTheme(payload));
			});
		} catch {
			// Not under Tauri; nothing to subscribe to.
		}

		try {
			// macOS / Windows deliver runtime flips through the webview
			// directly, so this covers what the Linux watcher doesn't.
			const win = getCurrentWindow();
			await win.onThemeChanged(({ payload }) => {
				this.updateSystemPrefersDark(payload === 'dark');
			});
		} catch {
			// Not running under Tauri; change notifications are
			// matchMedia-only.
		}
	}

	private updateSystemPrefersDark(next: boolean) {
		if (this.systemPrefersDark === next) {
			return;
		}
		this.systemPrefersDark = next;
		if (this.theme === 'system') {
			applyTheme(this.effectiveTheme);
		}
	}

	private systemPreferenceBound = false;

	flash(msg: string) {
		this.toast = msg;
		setTimeout(() => {
			if (this.toast === msg) {
				this.toast = null;
			}
		}, 4000);
	}
}

/**
 * Paint `mode` (always resolved dark/light, never `'system'`) on
 * `:root`. The `.light` class flip is the one stylesheet-global
 * signal — CSS variables defined in `styles.css` key off it and
 * every component reads through them, so one class toggle reskins
 * the whole IDE.
 */
function applyTheme(mode: 'dark' | 'light') {
	const root = document.documentElement;
	if (mode === 'light') {
		root.classList.add('light');
	} else {
		root.classList.remove('light');
	}
	// Stash the resolved palette so `index.html`'s inline boot script
	// can pick the right background on the very next launch — before
	// JS even parses. Resolved value (not the `'system'` choice),
	// because the boot splash has no ashpd / `matchMedia` yet.
	try {
		localStorage.setItem('moon-theme-applied', mode);
	} catch {
		// Storage disabled — boot splash just defaults to dark next
		// time. Not worth surfacing.
	}
	// The editor scrollbar corner (the moon easter egg) does NOT follow
	// theme toggles on WebKitGTK — see the note in `lib/editor/theme.ts`
	// for the full list of invalidation paths we tried. We accept the
	// stale-corner-after-toggle behaviour rather than ship more hacks.
}

function detectSystemPrefersDark(): boolean {
	if (typeof window === 'undefined' || typeof window.matchMedia !== 'function') {
		return true;
	}
	return window.matchMedia('(prefers-color-scheme: dark)').matches;
}

function resolveSystemTheme(theme: SystemTheme): boolean {
	// `unspecified` comes from the XDG portal when the user hasn't
	// set a preference at all. Mapping it to dark keeps moon-ide's
	// default chrome (dark) visible on a fresh desktop instead of
	// flashing to light.
	if (theme === 'unspecified') {
		return true;
	}
	return theme === 'dark';
}

/**
 * How deep `fs_collect_paths` recurses. Cap exists so very deep
 * trees can't stall the UI on first load or on refresh. Entries
 * beyond the cap are listed at their level but their children
 * aren't enumerated — Phase 1 will add lazy expansion to remove
 * this cap, at which point the constant goes away.
 */
const MAX_TREE_DEPTH = 6;

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
function buildDiscardMessage(toRestore: string[], toTrash: string[]): string {
	const total = toRestore.length + toTrash.length;
	if (total === 1) {
		if (toRestore.length === 1) {
			const p = toRestore[0] ?? '';
			return `Discard your changes to ${p}? This cannot be undone.`;
		}
		const p = toTrash[0] ?? '';
		const isDir = p.endsWith('/');
		const noun = isDir ? 'untracked folder' : 'untracked file';
		return `Move the ${noun} ${p} to the trash? This deletes it since git has nothing to restore it to.`;
	}
	const parts: string[] = [];
	if (toRestore.length > 0) {
		parts.push(`restore ${toRestore.length} tracked file${toRestore.length === 1 ? '' : 's'} to HEAD`);
	}
	if (toTrash.length > 0) {
		const dirs = toTrash.filter((p) => p.endsWith('/')).length;
		const files = toTrash.length - dirs;
		const untrackedParts: string[] = [];
		if (files > 0) {
			untrackedParts.push(`${files} file${files === 1 ? '' : 's'}`);
		}
		if (dirs > 0) {
			untrackedParts.push(`${dirs} folder${dirs === 1 ? '' : 's'}`);
		}
		parts.push(`move ${untrackedParts.join(' and ')} to the trash`);
	}
	return `Discard ${total} change${total === 1 ? '' : 's'}? This will ${parts.join(' and ')}. The restore step cannot be undone.`;
}

/**
 * Whether the user's pending removal is safe to execute without a
 * confirm dialog. "Safe" here means git can un-do the removal with
 * one command: for a file, that it's tracked-clean; for a folder,
 * that no descendant carries a non-ignored status. The moment a
 * dirty, untracked, or outside-a-repo path shows up we fall back to
 * the dialog — the user's only recovery there is the OS trash
 * (reversible but annoying) or nothing at all (permanent delete),
 * and an accidental keypress shouldn't silently commit to either.
 *
 * The presence of `gitStatusEntries` is also our "am I in a repo?"
 * proxy: outside a repo the backend emits only ignored entries
 * (walker fallback), which we treat as unsafe by default because
 * git has no recovery story to offer.
 */
function canSkipRemovalConfirm(realPaths: readonly string[], entries: readonly GitStatusEntry[]): boolean {
	if (entries.length === 0) {
		return false;
	}
	const hasDirty = entries.some((e) => e.status !== 'ignored');
	if (!hasDirty) {
		// An all-ignored entry set means we never saw a real git
		// status signal — treat as "not a repo" from a safety
		// standpoint.
		return false;
	}
	for (const path of realPaths) {
		if (path.endsWith('/')) {
			for (const entry of entries) {
				if (entry.status === 'ignored') {
					continue;
				}
				if (entry.path === path || entry.path.startsWith(path)) {
					return false;
				}
			}
			continue;
		}
		for (const entry of entries) {
			if (entry.path === path && entry.status !== 'ignored') {
				return false;
			}
		}
	}
	return true;
}

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
