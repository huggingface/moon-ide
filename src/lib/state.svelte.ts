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
	type BranchList,
	type PrListScope,
	type CompareBaseline,
	type BranchSwitchTarget,
	type CommitEntry,
	type EditorConfig,
	type EditRequest,
	type FolderSession,
	type GitBranchInfo,
	type GitChangeSummary,
	type GitFileBlame,
	type GitMergeState,
	type GitStatusEntry,
	type LspDiagnostic,
	type LspDiagnosticsEvent,
	type LspStatusEvent,
	type NextEditProbeResult,
	type NextEditServerSnapshot,
	type PublishReviewResult,
	type ReviewComment,
	type ReviewedFile,
	type ReviewSide,
	type SplitSide,
	type SystemTheme,
	type ThemeMode,
	type Workspace,
	type WorkspaceFolder,
	type WorkspaceSession,
} from './protocol';
import { lspLanguageFor, lspSlotCoversFile } from './editor/lspLanguage';
import { bottomPanel } from './bottomPanel.svelte';
import { composeLogs } from './composeLogs.svelte';
import { diagLogs, frontendLog } from './logs.svelte';
import { terminal } from './terminal.svelte';
import { coder } from './coder.svelte';
import { container } from './container.svelte';
import {
	canOpenContainerTerminal,
	ensureActiveFolderTerminal,
	forgetTerminalMemoryFor,
	openContainerTerminal,
	openHostTerminal,
	rememberActiveTerminalFor,
} from './openTerminal';
import { ports } from './ports.svelte';
import { projectCompose } from './projectCompose.svelte';
import { rightPanel } from './rightPanel.svelte';
import { slack } from './slack.svelte';
import { fingerprint, fingerprintEquals, type ContentFingerprint } from './util/hash';
import { fileKindFor, type FileKind } from './util/fileKind';
import { isMarkdownPath } from './util/markdown';
import { isReviewPath, REVIEW_PATH } from './util/reviewPath';
import { commitPath, isCommitPath, shaFromCommitPath } from './util/commitPath';

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
	// Tauri asset:// URL for binary preview files (images, PDFs); empty
	// string for text and untitled buffers. Text and untitled buffers
	// stream their contents through `text`, preview files render via this
	// URL.
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
	// Non-null when this buffer is parked waiting on a forwarded
	// `$GIT_EDITOR` invocation from a container terminal — the
	// `moon-edit` shim is blocked on the per-workspace
	// `instance.sock` until the user finishes (Ctrl+S, or the
	// tab's "Finish editing" affordance) or cancels (close the
	// tab without saving). The value is the opaque `EditId` the
	// backend sent in the `editor:request` event; we round-trip
	// it through `ipc.editorForward.finish` / `.cancel`. See
	// ADR 0021 and `specs/containers.md` § "Editor forwarding".
	pendingEdit: string | null;
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

	// Directories the backend stopped recursion at because we hit
	// `MAX_TREE_DEPTH`. The file tree treats these like ignored
	// directories: the row is shown, but its children are fetched
	// on expansion via `collect_paths_under` instead of being
	// enumerated up-front. Empty for shallow projects where the
	// depth cap never kicked in.
	depthCappedPaths = $state<string[]>([]);

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

	// Branch-switcher PR-section filter for this folder. Persisted
	// per folder (`FolderSession.pr_scope`) so a busy monorepo can
	// stay in `participating` mode while a side-project keeps the
	// default `all`. Defaults to `all`.
	prScope = $state<PrListScope>('all');

	// SCM compare baseline for this folder. `'head'` is the
	// regular `git status` against HEAD; `'default'` swaps in the
	// merge-base with the repo's default branch so the file
	// tree, gutter, and diff view all show "what's different
	// from main". Persisted in `FolderSession.compare_baseline`.
	compareBaseline = $state<CompareBaseline>('head');

	// Cached merge-base SHA when `compareBaseline === 'default'`
	// and the diff actually applies (resolved default branch,
	// HEAD off the default branch, merge-base exists). `null`
	// signals "applicable" (host returned `None`) — the
	// frontend keeps `compareBaseline === 'default'` so the
	// toggle stays sticky, but every consumer falls back to
	// `'head'` semantics. Re-derived on every
	// `refreshGitStatus` pass.
	defaultBranchMergeBase = $state<string | null>(null);

	// Human-readable name of the default branch we last
	// resolved against (e.g. `'origin/main'`). Powers the SCM
	// panel header's `vs main` / `vs master` label so the
	// toggle reads correctly on `master`-defaulted repos.
	// `null` mirrors `defaultBranchMergeBase`'s
	// "not-applicable" semantics.
	defaultBranchName = $state<string | null>(null);

	// Commit-message draft for this folder's SCM panel. Survives
	// folder switches so the user can flip between projects
	// mid-sentence without losing their commit message — same
	// rationale as keeping editor tabs and SCM compare baseline
	// per folder. In-memory only (not in `FolderSession` on
	// disk); IDE restart clears the draft, which matches how
	// editor scratch buffers behave today.
	commitDraft = $state('');

	// Scroll-restore snapshot for this folder's Review changes
	// pseudo-tab. `ReviewView` writes it on unmount (the file whose
	// section was nearest the top of the viewport plus the pixel
	// offset into that section) and reads it back on the next mount,
	// so switching away — to another tab *or another folder* — and
	// coming back lands the reader where they left off instead of
	// scrolling to the top and re-lazy-loading every section.
	// Per-folder so each folder's review keeps its own position; the
	// review tab buffer itself already lives in this folder's
	// `openFiles`, so it survives a folder switch the same way.
	// In-memory only (not persisted): cleared when the review tab is
	// actually closed (see `closeFile`). Keyed by path so a baseline
	// flip — which remounts the sections under a new key but keeps
	// the same `review://` tab — still restores.
	reviewRestore = $state<{ path: string; offset: number } | null>(null);

	// Local-first review-comment drafts for this folder (Phase
	// 5.7). Anchored by content so they survive edits and rebases;
	// persisted in `FolderSession.review_comments` and cleared once
	// published to a GitHub PR. CRUD goes through `WorkspaceState`,
	// which proxies the active folder.
	reviewComments = $state<ReviewComment[]>([]);

	// Per-file "Viewed" marks for this folder (Phase 5.7). Each is
	// pinned to the blob SHA of the version that was ticked;
	// `refreshReviewedFiles` drops entries whose file changed.
	// Persisted in `FolderSession.reviewed_files`. Never published.
	reviewedFiles = $state<ReviewedFile[]>([]);

	constructor(public readonly folderPath: string) {}
}

/**
 * Shape of a single `coder:event` Tauri event for the project-bar
 * refresh listener. Mirrors the parts of `CoderEvent` we actually
 * inspect — the rest stays unknown so the type is honest about
 * what we don't introspect.
 */
type CoderRefreshEvent = {
	kind: string;
	name?: string;
	args?: Record<string, unknown> | null;
	inner?: { kind?: string };
};
type CoderRefreshEnvelope = {
	folder: string;
	event: CoderRefreshEvent;
};

class WorkspaceState {
	workspace = $state<Workspace | null>(null);

	// Per-folder UI state. Keyed by absolute folder path — same as
	// `Workspace.active_folder` and `WorkspaceFolder.path`. Survives
	// folder switches: switching to a folder whose state already
	// exists rebinds the proxied accessors below to that folder's
	// buffers / tabs without rebuilding them.
	folderStates = new SvelteMap<string, FolderState>();

	// Monotonic "editor view changed" signal. Bumped by every setter
	// that mutates what an editor pane renders (open buffers, active
	// path, focused side, diff/preview mode). `EditorPane`'s `view`
	// derived reads this first thing so it has a guaranteed-fresh
	// top-level dependency that *always* fires on navigation —
	// independent of the `activeFolderState` getter funnel.
	//
	// Why this exists: reading per-folder fields goes
	// `workspace.leftActive` → `activeFolderState` (resolves the
	// `folderStates` map entry) → `.leftActive` on that `FolderState`
	// instance. Empirically a consumer's subscription to the leaf
	// field could go stale (the editor body froze on a buffer that
	// was no longer even open, while the tab strip kept updating).
	// Rather than chase the exact fine-grained-reactivity edge, this
	// tick gives the body a dependency that can't be lost: a plain
	// top-level `$state` the consumer re-reads every run and every
	// mutating setter bumps. The watchdog also bumps it as a recovery
	// nudge. Cheap (one integer) and the editor view is not a hot
	// enough recompute for it to matter.
	editorViewTick = $state(0);

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

	// Soft-wrap toggle for every editor pane. Off by default (matches
	// VS Code / Cursor) so source code stays on its tab-aligned grid;
	// `Alt+Z` flips it on, which is the canonical use case when looking
	// at long-lined buffers (coder session JSONL traces, minified
	// output, log dumps). Window-global rather than per-buffer because
	// the team only flips it occasionally — per-buffer state would
	// pull in an extra map on the persistence layer for ~zero benefit.
	// Not persisted across restarts: the on-vs-off cost is one
	// keystroke and the team is small.
	lineWrap = $state(false);

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

	// LSP diagnostics per path, flat union across all producers.
	// Full-replacement semantics applies *per producer*, not per
	// path — `lsp:diagnostics` events stamp `producer` (the server's
	// slot key, e.g. `"typescript"` or `"oxlint"`) so two co-tenant
	// servers reporting on the same file each refresh their own
	// slice without clobbering the other. The flat union is what the
	// editor's lint gutter and the status bar read; the per-producer
	// split lives in [`diagnosticsByProducer`] so we can refresh one
	// slice cleanly when a single server publishes.
	diagnostics = $state<Map<string, LspDiagnostic[]>>(new Map());

	// Backing per-producer state for [`diagnostics`]. Keyed first
	// by file path, then by `producer` from the `lsp:diagnostics`
	// event. Outer Map is a fresh object on update so Svelte
	// reactivity reaches into derived consumers; the inner Map is
	// rebuilt on each producer-update too. Held alongside
	// `diagnostics` (rather than computed lazily) because every
	// editor frame would otherwise reduce-then-flatten on the hot
	// path.
	diagnosticsByProducer = $state<Map<string, Map<string, LspDiagnostic[]>>>(new Map());

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
	/**
	 * Debounce timer for the focus-driven LSP diagnostic refresh
	 * (`ipc.lsp.refreshOpenDiagnostics([])`). Window-focus can fire
	 * twice in quick succession (alt-tab → click into the IDE → focus
	 * event for each), so the 250ms timer collapses them. In-IDE
	 * off-disk changes go through `ipc.lsp.notifyFilesChanged`
	 * instead — the LSP server itself decides when to ask us for a
	 * refresh, so we don't need a client-side scheduler for that
	 * path.
	 */
	#lspRefreshTimer: ReturnType<typeof setTimeout> | null = null;

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

	// "Scroll-to-section" signal for the Review changes pseudo-tab.
	// When the review tab is active in some pane and the user clicks
	// a file row in the SCM changes tree, we bump this with the
	// clicked path; `ReviewView` watches it and scrolls its matching
	// section into view. `tick` makes repeated clicks on the same
	// row re-trigger the effect — without it Svelte would dedupe the
	// reactive update and the second click would feel broken.
	reviewScrollRequest = $state<{ path: string; tick: number } | null>(null);

	// Whichever per-file diff section is currently centred in the
	// Review changes pseudo-tab. `ReviewView` keeps this updated
	// as the user scrolls; `toggleReviewTab` reads it on close to
	// jump the user straight to that file's regular editor tab.
	// `null` when no review tab is currently open or the review
	// stack is empty.
	reviewVisibleFile = $state<string | null>(null);

	// Scroll-restore snapshot for the Review changes pseudo-tab,
	// stored per-folder on `FolderState` (see the field there for the
	// full rationale). `ReviewView` reads/writes it against the
	// folder path it captured at mount — *not* via the active-folder
	// proxy — because the snapshot is written from `onDestroy`, which
	// on a folder switch fires *after* `active_folder` has already
	// flipped to the new folder. Routing through the proxy would
	// stash the old folder's scroll position under the new folder's
	// state. Keying explicitly avoids that race.
	reviewRestoreFor(folder: string | null): { path: string; offset: number } | null {
		if (folder === null) {
			return null;
		}
		return this.folderStates.get(folder)?.reviewRestore ?? null;
	}
	setReviewRestoreFor(folder: string | null, value: { path: string; offset: number } | null): void {
		if (folder === null) {
			return;
		}
		const fs = this.folderStates.get(folder);
		if (fs) {
			fs.reviewRestore = value;
		}
	}

	// Path of the review section whose CodeMirror editor currently
	// holds focus. `ReviewSection` updates this on focus / blur of
	// its right-side editor; cleared when focus leaves the review
	// surface entirely. `saveActive` checks this when the active
	// tab is a `review://` buffer so `Ctrl+S` lands on the file
	// the user is editing, not on the synthetic review buffer
	// itself.
	reviewFocusPath = $state<string | null>(null);

	// --- Review comments (Phase 5.7) -----------------------------------
	// All comment CRUD proxies the active folder's `FolderState`, the
	// same shape as the `compareBaseline` / `reviewRestore` proxies.
	// Every mutation persists the session so drafts survive a restart.

	/** Comment drafts for the active folder. Empty when no folder. */
	get reviewComments(): readonly ReviewComment[] {
		return this.activeFolderState?.reviewComments ?? [];
	}

	/** Comment drafts anchored to `path`, in creation order. */
	reviewCommentsForPath(path: string): ReviewComment[] {
		return (this.activeFolderState?.reviewComments ?? []).filter((c) => c.anchor.path === path);
	}

	/**
	 * Create a comment anchored to `path` lines `startLine..=endLine`
	 * on `side`. `lineText` is the trimmed source of the anchored
	 * line(s), used to compute the content fingerprint the publish
	 * path and the re-anchoring pass key off. `baselineRev` is the
	 * merge-base / HEAD SHA the review is being read against.
	 */
	addReviewComment(args: {
		path: string;
		side: ReviewSide;
		startLine: number;
		endLine: number;
		lineText: string;
		baselineRev: string;
		body: string;
	}): ReviewComment | null {
		const fs = this.activeFolderState;
		if (!fs) {
			return null;
		}
		const comment: ReviewComment = {
			id: crypto.randomUUID(),
			anchor: {
				path: args.path,
				side: args.side,
				startLine: args.startLine,
				endLine: args.endLine,
				fingerprint: reviewFingerprint(args.lineText),
				baselineRev: args.baselineRev,
			},
			body: args.body,
			createdAt: new Date().toISOString(),
		};
		fs.reviewComments = [...fs.reviewComments, comment];
		this.persistAppState();
		return comment;
	}

	/** Replace a comment's body. No-op if the id is unknown. */
	editReviewComment(id: string, body: string): void {
		const fs = this.activeFolderState;
		if (!fs) {
			return;
		}
		const next = fs.reviewComments.map((c) => (c.id === id ? { ...c, body } : c));
		if (next.some((c, i) => c !== fs.reviewComments[i])) {
			fs.reviewComments = next;
			this.persistAppState();
		}
	}

	/** Delete a comment by id. No-op if unknown. */
	deleteReviewComment(id: string): void {
		const fs = this.activeFolderState;
		if (!fs) {
			return;
		}
		const next = fs.reviewComments.filter((c) => c.id !== id);
		if (next.length !== fs.reviewComments.length) {
			fs.reviewComments = next;
			this.persistAppState();
		}
	}

	/**
	 * Whether the active folder is in a state where leaving review
	 * comments makes sense outside the Review tab: an open PR for
	 * the branch, or any branch that isn't the default. Gates the
	 * comment affordances in the regular editor and the diff view —
	 * the Review changes tab stays ungated (opening it is already an
	 * explicit "I'm reviewing" signal).
	 */
	get isReviewableBranch(): boolean {
		const b = this.gitBranch;
		if (b.name === null) {
			return false;
		}
		if (b.prUrl !== null) {
			return true;
		}
		const def = b.defaultBranchRemoteRef;
		if (def === null) {
			return false;
		}
		// `origin/main` → `main`; keeps nested branch names intact
		// (`origin/feature/x` → `feature/x`).
		const short = def.includes('/') ? def.slice(def.indexOf('/') + 1) : def;
		return short !== b.name;
	}

	/**
	 * Publish the active folder's review-comment drafts to the
	 * current branch's GitHub PR as one review (Phase 5.7.2). On a
	 * successful post, the comments that landed (everything except
	 * the "lost" ids) are deleted locally — drafts only live until
	 * they're on GitHub. Returns the backend result so the dialog can
	 * report posted / lost / no-PR. `null` when there's no active
	 * folder or no comments to publish.
	 */
	async publishReview(body: string | null): Promise<PublishReviewResult | null> {
		const fs = this.activeFolderState;
		if (!fs || fs.reviewComments.length === 0) {
			return null;
		}
		const comments = [...fs.reviewComments];
		const result = await ipc.fs.publishPrReview({ body, comments });
		if (result.kind === 'published' && this.activeFolderState === fs) {
			// Drop the comments that posted; keep the lost ones as
			// local drafts so the user can retry / re-place them.
			const lost = new Set(result.lost);
			const submitted = new Set(comments.map((c) => c.id));
			fs.reviewComments = fs.reviewComments.filter((c) => !submitted.has(c.id) || lost.has(c.id));
			this.persistAppState();
		}
		return result;
	}

	/**
	 * Re-pin a comment's line hint after the re-anchoring pass found
	 * its fingerprint at a new location. Doesn't touch the
	 * fingerprint or body. Persisted so the hint is fresh next launch.
	 */
	repinReviewComment(id: string, startLine: number, endLine: number): void {
		const fs = this.activeFolderState;
		if (!fs) {
			return;
		}
		const next = fs.reviewComments.map((c) =>
			c.id === id ? { ...c, anchor: { ...c.anchor, startLine, endLine } } : c,
		);
		if (next.some((c, i) => c !== fs.reviewComments[i])) {
			fs.reviewComments = next;
			this.persistAppState();
		}
	}

	// --- Reviewed-file marks (Phase 5.7) -------------------------------

	/** Reviewed-file marks for the active folder. */
	get reviewedFiles(): readonly ReviewedFile[] {
		return this.activeFolderState?.reviewedFiles ?? [];
	}

	/** Whether `path` is currently marked reviewed in the active folder. */
	isFileReviewed(path: string): boolean {
		return (this.activeFolderState?.reviewedFiles ?? []).some((r) => r.path === path);
	}

	/**
	 * Mark `path` reviewed, pinned to its current blob SHA (resolved
	 * via the host). A later `refreshReviewedFiles` clears the mark
	 * if the file's SHA moves. Unticking is just removal. Resolving
	 * the SHA can fail (file gone, no git); we no-op rather than store
	 * a mark we can't validate.
	 */
	async setFileReviewed(path: string, reviewed: boolean): Promise<void> {
		const fs = this.activeFolderState;
		if (!fs) {
			return;
		}
		if (!reviewed) {
			const next = fs.reviewedFiles.filter((r) => r.path !== path);
			if (next.length !== fs.reviewedFiles.length) {
				fs.reviewedFiles = next;
				this.persistAppState();
			}
			return;
		}
		const sha = await ipc.fs.gitBlobSha(path);
		if (sha === null) {
			return;
		}
		// The folder could have switched while the SHA resolved.
		if (this.activeFolderState !== fs) {
			return;
		}
		const mark: ReviewedFile = { path, reviewedRev: sha, reviewedAt: new Date().toISOString() };
		fs.reviewedFiles = [...fs.reviewedFiles.filter((r) => r.path !== path), mark];
		this.persistAppState();
	}

	/**
	 * Drop reviewed-file marks whose pinned blob SHA no longer matches
	 * the file's current SHA — i.e. the file changed (local edit, new
	 * local commit, or pulled commit) since it was ticked, so the
	 * "Viewed" state is stale and the row should re-surface for
	 * re-review. Called from the git-status refresh. Resolves each
	 * marked file's current blob SHA via the host (cheap — only the
	 * handful of files that are actually marked). A file that no
	 * longer resolves (deleted / unreadable) also clears.
	 */
	async refreshReviewedFiles(): Promise<void> {
		const fs = this.activeFolderState;
		if (!fs || fs.reviewedFiles.length === 0) {
			return;
		}
		const marks = fs.reviewedFiles;
		const shas = await Promise.all(marks.map((r) => ipc.fs.gitBlobSha(r.path).catch(() => null as string | null)));
		// The folder could have switched while SHAs resolved.
		if (this.activeFolderState !== fs) {
			return;
		}
		const kept = fs.reviewedFiles.filter((r, i) => shas[i] === r.reviewedRev);
		if (kept.length !== fs.reviewedFiles.length) {
			fs.reviewedFiles = kept;
			this.persistAppState();
		}
	}

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
	 * Display name of the workspace this window belongs to —
	 * e.g. `"Hugging Face"` for a workspace whose slug is
	 * `huggingface`. Looked up from the catalog once on hydrate
	 * and cached; the title bar reads from here. `null` until
	 * the catalog fetch resolves.
	 */
	workspaceName: string | null = $state(null);

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
	// Plain getter, deliberately *not* a `$derived`. A cached
	// `$derived<FolderState>` was the source of a nasty stale-read
	// bug: a consumer (e.g. `EditorPane`'s view derived) would read
	// `workspace.leftActive` → `activeFolderState.leftActive`, but if
	// `activeFolderState`'s cached object reference lagged the
	// folder/map state for a tick, the consumer ended up subscribed
	// to the wrong `FolderState` instance's field — so a later
	// `setActive` mutating the *correct* instance never notified it,
	// and the editor body froze on the old buffer until a folder
	// swap rebuilt the graph. Resolving the lookup inline on every
	// access means each caller subscribes directly to
	// `activeFolderPath` + the `folderStates` map entry + the leaf
	// `$state` field it reads — no intermediate cached reference to
	// desync. The map lookup is O(1) and these getters aren't hot
	// enough for the allocation-free derived to matter.
	private get activeFolderState(): FolderState | null {
		const path = this.activeFolderPath;
		if (path === null) {
			return null;
		}
		return this.folderStates.get(path) ?? null;
	}

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

	get depthCappedPaths(): string[] {
		return this.activeFolderState?.depthCappedPaths ?? [];
	}
	set depthCappedPaths(value: string[]) {
		if (this.activeFolderState) {
			this.activeFolderState.depthCappedPaths = value;
		}
	}

	get openFiles(): OpenFile[] {
		return this.activeFolderState?.openFiles ?? [];
	}
	set openFiles(value: OpenFile[]) {
		if (this.activeFolderState) {
			this.activeFolderState.openFiles = value;
			this.editorViewTick++;
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
			this.editorViewTick++;
		}
	}

	get rightActive(): string | null {
		return this.activeFolderState?.rightActive ?? null;
	}
	set rightActive(value: string | null) {
		if (this.activeFolderState) {
			this.activeFolderState.rightActive = value;
			this.editorViewTick++;
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
			this.editorViewTick++;
		}
	}

	private get previewModes(): Map<string, MarkdownView> {
		return this.activeFolderState?.previewModes ?? new Map();
	}
	private set previewModes(value: Map<string, MarkdownView>) {
		if (this.activeFolderState) {
			this.activeFolderState.previewModes = value;
			this.editorViewTick++;
		}
	}

	private get diffModes(): Set<string> {
		return this.activeFolderState?.diffModes ?? new Set();
	}
	private set diffModes(value: Set<string>) {
		if (this.activeFolderState) {
			this.activeFolderState.diffModes = value;
			this.editorViewTick++;
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

	/**
	 * Active folder's commit-message draft. The SCM panel binds
	 * its textarea to this so flipping between folders doesn't
	 * lose half-typed commit messages — same per-folder
	 * persistence model as editor tabs and PR filter. Returns
	 * `''` when no folder is active; writes are silent no-ops in
	 * that case (the SCM panel doesn't render without a folder
	 * anyway).
	 */
	get commitDraft(): string {
		return this.activeFolderState?.commitDraft ?? '';
	}
	set commitDraft(value: string) {
		if (this.activeFolderState) {
			this.activeFolderState.commitDraft = value;
		}
	}

	activePath: string | null = $derived(this.focusedSide === 'left' ? this.leftActive : this.rightActive);

	activeFile: OpenFile | null = $derived.by(() => {
		// Guaranteed-fresh invalidation, same rationale as
		// `EditorPane`'s `view` derived: the per-folder field reads
		// below route through the `activeFolderState` getter funnel,
		// whose leaf-field subscription can go stale. The tick can't.
		void this.editorViewTick;
		// Read `openFiles` before the early-out so this derived stays
		// subscribed to the buffer list even when there's no active
		// path yet (fresh folder open). Otherwise a later populate of
		// `openFiles` + `activePath` wouldn't re-run this derived and
		// every consumer would keep seeing `null`. See the matching
		// note in `EditorPane.svelte`'s `view` derived.
		const openFiles = this.openFiles;
		const path = this.activePath;
		if (path === null) {
			return null;
		}
		return openFiles.find((f) => f.path === path) ?? null;
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
	 * Per-buffer view-state cache for tab switches. Keyed by
	 * `${folder}::${path}` like `navStack`. Holds the caret
	 * offset, the selection's anchor offset (so a range
	 * survives, not just a cursor), the scroller's `scrollTop`,
	 * and the serialized CodeMirror history (so Ctrl+Z still
	 * walks back through edits made *before* the user clicked
	 * away to another tab). `Editor.svelte` snapshots into here
	 * right before tearing down its `EditorView` state for a tab
	 * swap, and reads back when re-mounting the same path. The
	 * cache entry survives any number of switches; it's
	 * dropped only when the buffer falls out of every pane
	 * (see `closeFile`'s GC block).
	 *
	 * `historyJson` is the output of
	 * `EditorState.toJSON({ history: historyField })` (the
	 * `history` slot only — doc / selection are restored
	 * separately because the workspace's `file.text` is the
	 * authoritative doc and a stray external edit can shift
	 * the in-buffer offsets without invalidating the history's
	 * delta chain). The shape is `unknown` because CM doesn't
	 * publish the schema — we only ever hand it back to
	 * `EditorState.fromJSON`.
	 *
	 * Deliberately **not** reactive — cursor moves don't need
	 * to wake up the reactive graph, and the snapshot/restore
	 * cycle is driven by lifecycle hooks, not reads from
	 * Svelte components.
	 */
	private viewStateByKey = new Map<
		string,
		{ caretOffset: number; anchorOffset: number; scrollTop: number; historyJson?: unknown }
	>();

	snapshotViewState(
		folder: string,
		path: string,
		snapshot: { caretOffset: number; anchorOffset: number; scrollTop: number; historyJson?: unknown },
	): void {
		this.viewStateByKey.set(navKey(folder, path), snapshot);
	}

	getViewState(
		folder: string,
		path: string,
	): { caretOffset: number; anchorOffset: number; scrollTop: number; historyJson?: unknown } | null {
		return this.viewStateByKey.get(navKey(folder, path)) ?? null;
	}

	private dropViewState(folder: string, path: string): void {
		this.viewStateByKey.delete(navKey(folder, path));
	}

	/**
	 * Add `path` as a folder in the workspace and make it active.
	 * Idempotent on duplicate path — the backend silently flips the
	 * existing entry to active, and we re-load its tree if it had
	 * never been populated. Per Phase 2.5 this is the single
	 * `Add folder` code path: the welcome screen, the sidebar's
	 * `+ Add folder` row, the command palette's `Add Folder…`, and
	 * the `EditorPane` empty-state button all funnel through here.
	 */
	async openLocal(path: string) {
		const beforeCount = this.workspace?.folders.length ?? 0;
		const previousFolderPath = this.activeFolderPath;
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
			// Mirror the folder-switch terminal routing so adding a
			// new folder behaves like switching to it: remember the
			// outgoing folder's selected terminal, then ensure a
			// shell is ready in the new folder. The new folder has
			// no prior memory and no cwd match, so this resolves to
			// "spawn a fresh terminal" — but only when the bottom
			// panel already hosts a terminal, matching the gate
			// `ensureActiveFolderTerminal` already enforces.
			rememberActiveTerminalFor(previousFolderPath);
			ensureActiveFolderTerminal();
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
		// Snapshot the currently-active bottom-panel terminal (if
		// any) as the remembered pick for the folder we're leaving,
		// so a later return to that folder restores the same pane
		// rather than just the first cwd-match. No-op when the user
		// is on a non-terminal tab or when there's no prior folder.
		const previousFolderPath = this.activeFolderPath;
		// Profiling: a marker the user can wrap a DevTools >
		// Performance > Record around to scope the flame chart to a
		// single folder-switch gesture. The `performance.measure`
		// pairs below show up as named bars under the "User Timing"
		// track. Paired with the post-rAF entries in
		// `loadPaths` / FileTree.svelte we get a full timeline:
		// click → IPC → sync adopt → reactive cascade → tree
		// rebuild → git status refresh. See test plan 0076 for the
		// reading guide.
		performance.mark('moon:setActiveFolder.start');
		const tStart = performance.now();
		try {
			const ws = await ipc.workspace.setActiveFolder(path);
			const tIpc = performance.now();
			performance.measure('moon:setActiveFolder.ipc', 'moon:setActiveFolder.start');
			performance.mark('moon:setActiveFolder.adopt.start');
			await this.adoptWorkspaceSnapshot(ws);
			const tAdopt = performance.now();
			performance.measure('moon:setActiveFolder.adopt', 'moon:setActiveFolder.adopt.start');
			this.persistAppState();
			// Schedule a follow-up timing capture once the browser
			// has had a chance to paint the new folder bar /
			// breadcrumb. rAF fires after the next style + layout +
			// paint commit, so this approximates "time from click
			// to first frame the user can see the new chrome". If
			// this is large despite a small `ipc + adopt`, the
			// stall is in Svelte's reactive cascade or in a
			// downstream effect (FileTree resetPaths, EditorPane
			// re-mount, SCM panel rebuild).
			requestAnimationFrame(() => {
				const tFirstFrame = performance.now();
				performance.mark('moon:setActiveFolder.firstFrame');
				performance.measure('moon:setActiveFolder.toFirstFrame', 'moon:setActiveFolder.start');
				// eslint-disable-next-line no-console
				console.info(
					`moon-ide: setActiveFolder(${path}) ` +
						`ipc=${(tIpc - tStart).toFixed(1)}ms ` +
						`adopt=${(tAdopt - tIpc).toFixed(1)}ms ` +
						`reactive+paint=${(tFirstFrame - tAdopt).toFixed(1)}ms ` +
						`toFirstFrame=${(tFirstFrame - tStart).toFixed(1)}ms`,
				);
			});
			// Re-prime the LSP for the new folder's open buffers.
			// The backend's `ensure_broker` rebuilds lazily on the
			// next `lsp_*` IPC (it sees the active-folder change
			// and tears down the old broker), but the freshly-built
			// broker starts with an empty docs map — no `didOpen`
			// has been fired for any of the new folder's tabs. In a
			// container-routed setup that often surfaces as TS
			// "Cannot find name 'assert'" / missing-`@types/node`
			// noise on the first interaction: typing fires
			// `lsp_update`, which reaches a server that doesn't
			// know the file (silently dropped), and surrounding
			// hover / completion / definition probes either get
			// nothing back or fall through to project-wide
			// analysis with stale assumptions. Mirrors what
			// `restartLsp` already does after a manual restart and
			// what `restoreAppState` does for the active folder at
			// startup — folder switches were the missing entry
			// point.
			for (const file of this.openFiles) {
				if (file.kind !== 'text' || file.isDeleted) {
					continue;
				}
				if (isSyntheticBufferPath(file.path)) {
					continue;
				}
				this.lspOpen(file.path, file.text);
			}
			// Kick an auto-fetch on the new folder so the Sync Changes
			// button surfaces promptly instead of waiting for the next
			// 3-minute periodic tick. Throttled internally — rapid
			// folder-switching doesn't spam fetches.
			void this.runGitAutoFetch('folder-switch');
			// If the bottom panel is open with a terminal, make sure
			// the user has a shell rooted in the new folder ready to
			// go — first the per-folder remembered terminal, then a
			// cwd match, finally a fresh spawn in the workspace's
			// preferred mode (container if up, host otherwise).
			// Hydration paths bypass this method (they call
			// `ipc.workspace.setActiveFolder` directly), so this
			// only fires on real user-driven switches.
			rememberActiveTerminalFor(previousFolderPath);
			ensureActiveFolderTerminal();
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
		// A worktree-backed session folder (ADR 0028) isn't a folder
		// the user picked — removing it prunes the git worktree, not
		// just the binding. Route to the dedicated discard flow.
		if (folder.origin.kind === 'worktree') {
			await this.discardWorktree(path);
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
			// Drop any remembered "this folder's preferred terminal"
			// id eagerly so the lazy-prune sweep in
			// `ensureActiveFolderTerminal` doesn't have to find it
			// later. Cheap idempotent delete.
			forgetTerminalMemoryFor(path);
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
	 * Spin up an isolated coder session in its own git worktree
	 * (ADR 0028). The backend branches off the active folder's HEAD,
	 * checks the branch out into a worktree, binds that worktree as
	 * a nested folder, and mints a session — filed under the
	 * still-active parent — whose tools route to the worktree. We
	 * adopt the returned snapshot so the nested folder row renders,
	 * then surface the new session in the panel.
	 *
	 * Container mounting of the worktree is deferred (ADR 0028 W.4),
	 * so this deliberately doesn't nudge `container.syncBoundFolders()`.
	 */
	async newCoderWorktreeSession(baseBranch?: string): Promise<void> {
		try {
			const result = await ipc.coder.newWorktreeSession(baseBranch);
			await this.adoptWorkspaceSnapshot(result.workspace);
			this.persistAppState();
			coder.adoptCreatedSession(result.session);
		} catch (err) {
			coder.surfaceError(err);
			this.flash(`Could not start isolated session: ${formatError(err)}`);
		}
	}

	/**
	 * Move the visible coder session into its own git worktree (ADR
	 * 0028). Unlike `newCoderWorktreeSession`, the conversation is
	 * preserved — only its summary refreshes (so the worktree chip
	 * appears) and the new worktree folder is bound. On a non-default
	 * branch the backend also resets the main tree to the default
	 * branch; a dirty tree is refused with git's message.
	 */
	async moveCoderSessionToWorktree(): Promise<void> {
		try {
			const result = await ipc.coder.moveSessionToWorktree();
			await this.adoptWorkspaceSnapshot(result.workspace);
			this.persistAppState();
			coder.adoptMovedSession(result.session);
		} catch (err) {
			this.flash(`Could not move session to a worktree: ${formatError(err)}`);
		}
	}

	/**
	 * Discard a worktree-backed session folder (ADR 0028): prune the
	 * git worktree and unbind it. The branch is kept — it's the
	 * deliverable, left for a PR. `git worktree remove` refuses a
	 * dirty worktree without `--force`, so a first failure prompts a
	 * second confirm before forcing.
	 */
	async discardWorktree(path: string) {
		const folder = this.workspace?.folders.find((f) => f.path === path);
		if (!folder || folder.origin.kind !== 'worktree') {
			return;
		}
		const branch = folder.origin.branch;
		const ok = await confirm(
			`Discard worktree on ${branch}?\n\nThe branch is kept — only the working copy is removed, so you can still open a PR from it later.`,
			{ title: 'Discard worktree', okLabel: 'Discard', cancelLabel: 'Cancel' },
		);
		if (!ok) {
			return;
		}
		try {
			await this.pruneWorktree(path, false);
		} catch (firstErr) {
			// Most likely the worktree has uncommitted or untracked
			// changes (git refuses without --force). Re-confirm, then
			// force. Any other error surfaces here too — the message
			// carries git's own stderr so the user knows what broke.
			const forceOk = await confirm(
				`This worktree has uncommitted or untracked changes that will be lost.\n\nDiscard them anyway?\n\n(${formatError(firstErr)})`,
				{ title: 'Discard worktree', okLabel: 'Discard changes', cancelLabel: 'Cancel' },
			);
			if (!forceOk) {
				return;
			}
			try {
				await this.pruneWorktree(path, true);
			} catch (secondErr) {
				this.flash(`Could not discard worktree: ${formatError(secondErr)}`);
			}
		}
	}

	private async pruneWorktree(path: string, force: boolean) {
		const ws = await ipc.coder.discardWorktree(path, force);
		this.folderStates.delete(path);
		this.pruneNavEntriesForFolder(path);
		projectCompose.forget(path);
		forgetTerminalMemoryFor(path);
		await this.adoptWorkspaceSnapshot(ws);
		this.persistAppState();
	}

	/**
	 * Merge a worktree's branch into the base (default) branch on
	 * the parent repo, then prune the worktree, delete the branch,
	 * and unbind the folder. Sessions that were routed to the
	 * worktree are cleared so they continue on the parent folder.
	 *
	 * If the merge fails (conflicts, dirty tree), the worktree is
	 * left intact and the error is surfaced so the user can resolve
	 * conflicts in the SCM panel and retry.
	 */
	async mergeAndRemoveWorktree(path: string, baseBranch: string) {
		const folder = this.workspace?.folders.find((f) => f.path === path);
		if (!folder || folder.origin.kind !== 'worktree') {
			return;
		}
		const branch = folder.origin.branch;
		const ok = await confirm(
			`Merge ${branch} into ${baseBranch} and remove the worktree?\n\nThe branch and working copy will be removed — the merged commits live on ${baseBranch}.`,
			{ title: 'Merge & remove worktree', okLabel: 'Merge & remove', cancelLabel: 'Cancel' },
		);
		if (!ok) {
			return;
		}
		try {
			const ws = await ipc.coder.mergeAndRemoveWorktree(path, baseBranch);
			this.folderStates.delete(path);
			this.pruneNavEntriesForFolder(path);
			projectCompose.forget(path);
			forgetTerminalMemoryFor(path);
			await this.adoptWorkspaceSnapshot(ws);
			this.persistAppState();
			this.flash(`Merged ${branch} into ${baseBranch} and removed worktree + branch.`);
		} catch (err) {
			this.flash(`Could not merge & remove worktree: ${formatError(err)}`);
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
		// Profiling: granular timings for the sync portion of the
		// hydration. `setActiveFolder`'s `adopt` line reports the
		// total of these plus whatever Svelte effects fire at the
		// `await adoptWorkspaceSnapshot()` microtask boundary. See
		// test plan 0076.
		const tAdopt0 = performance.now();
		performance.mark('moon:adopt.start');
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
			// Existing-PR pointer is folder-scoped: two folders on
			// the same branch name (commonly `main`) would otherwise
			// reuse the previous folder's cached result. Reset both
			// the URL and the "last queried branch" so the next
			// `refreshGitBranch` re-queries against the new folder's
			// remote.
			this.gitExistingPrUrl = null;
			this.gitExistingPrUrlBranch = null;
		}
		const tAdoptBlame = performance.now();
		this.workspace = snapshot.folders.length === 0 ? null : snapshot;
		const tAdoptAssign = performance.now();
		// Tell the coder panel which folder is now active so its
		// per-folder UI bucket flips. Per the multi-session design:
		// turns running in the previous folder keep going in the
		// background, just streaming events into their own bucket.
		// The user sees the new folder's transcript / sessions list
		// / draft / attachments restored intact when they return.
		//
		// Per-project session scoping (ADR 0028): a worktree folder
		// resolves to its parent project root, so a parent and all its
		// worktrees share one coder bucket (session list, transcript,
		// draft). Mirrors the backend's `coder_root_folder`.
		//
		// The actual active folder path (which may be a worktree) is
		// also forwarded so the coder panel can auto-switch to the
		// latest session associated with that specific worktree — or
		// the latest non-worktree session when the user switches back
		// to the parent.
		const active = snapshot.active_folder ?? null;
		const activeEntry = active !== null ? (snapshot.folders.find((f) => f.path === active) ?? null) : null;
		const coderRoot = activeEntry?.origin.kind === 'worktree' ? activeEntry.origin.parentPath : active;
		coder.setActiveFolder(coderRoot ?? null, active);
		const tAdoptCoder = performance.now();
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
		const tAdoptFolderStates = performance.now();
		// (Re)walk the active folder's tree on first visit and on
		// every folder switch. The switch case matters even when a
		// cached path list exists: the fs-watcher only ever watches
		// the *active* folder, so anything an external process (a
		// terminal `cp`, a script, another tool) created or deleted
		// in a non-active folder is invisible until we re-walk —
		// the cached snapshot can be arbitrarily stale. The cached
		// paths still paint instantly; the fresh walk reconciles in
		// the background and FileTree's structural-equality skip
		// makes the nothing-changed case free. `loadPaths` also
		// re-runs `refreshGitStatus` against the fresh paths, which
		// is what snaps the SCM panel header (branch / ahead /
		// behind) and the change-count badge to the new folder.
		//
		// `loadPaths()` is **fire-and-forget** here: awaiting it
		// would gate every caller of `adoptWorkspaceSnapshot` on a
		// full recursive backend walk, which on a many-thousand-file
		// project pins the IPC for hundreds of ms before the UI
		// can paint the new folder bar / breadcrumb / empty tree.
		// The frontend's `loadingPaths` flag covers the in-flight
		// window; the tree paints with the spinner first, then
		// reconciles when paths arrive.
		const activeFolderChanged = previousActive !== snapshot.active_folder;
		if (this.activeFolderState && (this.activeFolderState.paths.length === 0 || activeFolderChanged)) {
			void this.loadPaths();
		}
		// Warm the per-folder compose snapshot for every bound
		// folder so the bars paint with real data on first frame
		// (rather than blank-then-flash). Cheap when most folders
		// have no compose.yaml — the backend short-circuits to
		// `Absent` without invoking docker.
		void projectCompose.refreshAll(snapshot.folders.map((f) => f.path));
		// Same warm-up for the per-folder git change badges. Each
		// non-active folder's `git status` runs once here so the
		// project bar paints with real counts on first frame.
		// Subsequent refreshes ride on `refreshGitStatus` so an
		// agent edit in folder A also re-counts folder B.
		const gone = new Set(this.gitChangeSummaries.keys());
		for (const folder of snapshot.folders) {
			gone.delete(folder.path);
		}
		for (const path of gone) {
			this.gitChangeSummaries.delete(path);
		}
		void this.refreshAllGitChangeSummaries();
		const tAdoptEnd = performance.now();
		performance.mark('moon:adopt.end');
		performance.measure('moon:adopt', 'moon:adopt.start', 'moon:adopt.end');
		// eslint-disable-next-line no-console
		console.info(
			`moon-ide: adopt(${snapshot.active_folder ?? '<none>'}) ` +
				`blame=${(tAdoptBlame - tAdopt0).toFixed(1)}ms ` +
				`assignWs=${(tAdoptAssign - tAdoptBlame).toFixed(1)}ms ` +
				`coder=${(tAdoptCoder - tAdoptAssign).toFixed(1)}ms ` +
				`folderStates=${(tAdoptFolderStates - tAdoptCoder).toFixed(1)}ms ` +
				`tail=${(tAdoptEnd - tAdoptFolderStates).toFixed(1)}ms ` +
				`total=${(tAdoptEnd - tAdopt0).toFixed(1)}ms`,
		);
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
	 * Read the persisted AppState (theme + per-machine slices) plus the
	 * active workspace's session blob (folders, tabs, splits) and apply
	 * both. Theme is applied unconditionally. The session is only applied
	 * if it matches the currently-open workspace; tabs pointing at files
	 * that no longer exist are silently dropped and the cleaned-up state
	 * gets re-saved. Called once on startup from `App.svelte`.
	 */
	async restoreAppState() {
		// Register the coder→state callback so that opening a
		// worktree-backed session switches the folder bar to that
		// worktree. Done before any folder restore so the callback
		// is live by the time hydration might open a session.
		coder.registerWorktreeSessionCallback(async (worktreeRoot) => {
			await this.setActiveFolder(worktreeRoot);
		});
		// Probe the OS theme (XDG portal / native API) and read the
		// persisted `state.json` + per-workspace `session.json` in
		// parallel — they're independent and all block the first
		// meaningful paint. `bindSystemPreference` is awaited so
		// `applyTheme` below reads a trustworthy `systemPrefersDark`:
		// on Linux WebKitGTK the synchronous `matchMedia` answer
		// seeded during class construction is unreliable, and only
		// the portal probe knows for sure.
		const appStatePromise = ipc.appState.load().then(
			(state) => ({ ok: true, state }) as const,
			(err) => ({ ok: false, err }) as const,
		);
		const sessionPromise = ipc.session.load().then(
			(session) => ({ ok: true, session }) as const,
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
		const sessionResult = await sessionPromise;
		// A failed session load is not fatal — the user just lands
		// without their last tabs. The persist tick on first
		// interaction will heal the on-disk file. Soft-warn so it's
		// visible in dev without spooking the user.
		const session = sessionResult.ok ? sessionResult.session : null;
		if (!sessionResult.ok) {
			this.flash(`Could not restore session: ${formatError(sessionResult.err)}`);
		}

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
		// Workspace port forwards: same wire-and-refresh dance.
		// `ports.refresh` is cheap (one `docker inspect` for the
		// proxy + N `bind()` probes); calling it on every workspace
		// change keeps the panel's status dots fresh without
		// requiring it to be open.
		void ports.wireRuntime();
		void ports.refresh();
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
		// Diagnostic logs panel: subscribe to the backend's
		// `logs:entry` event so emits from the LSP broker and any
		// other future producer show up in the bottom-panel
		// picker without the user having to open the panel first.
		void diagLogs.start();
		this.wireNextEditProbe();
		this.wireGitAutoFetch();
		// Subscribe to forwarded `$GIT_EDITOR` requests from
		// in-container terminals so `git commit --amend` etc.
		// open a buffer in moon-ide. See ADR 0021.
		void this.wireEditorForward();
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
		// Coder event stream: drives the per-folder project-bar
		// badge refresh after a tool result lands, so cross-folder
		// agent edits show up without fs-watcher coverage of every
		// bound folder.
		void this.bindCoderRefresh();

		const ws = this.workspace;
		if (!ws || !session || session.folders.length === 0) {
			// Even without a session to replay we still want to give
			// the bottom-panel auto-spawn a shot — the panel's
			// visibility is in `state` (just hydrated above) and is
			// independent from the per-folder tab session restored
			// below.
			void this.spawnInitialBottomPanelTerminal(containerRefresh, terminalRuntime);
			// No folder loop to fight with — let the coder panel
			// hydrate the active folder immediately. Idempotent;
			// the `setActiveFolder` call earlier in the bootstrap
			// already queued a no-op hydrate that this flushes.
			coder.markWorkspaceReady();
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
							const file =
								kind === 'image' || kind === 'pdf'
									? await this.loadPreviewFile(path, kind)
									: await this.loadTextFile(path);
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
					// `pr_scope` defaulted server-side via
					// `#[serde(default)]` for older sessions that
					// don't carry the field; trust whatever lands.
					fs.prScope = folderSession.pr_scope ?? 'all';
					fs.compareBaseline = folderSession.compare_baseline ?? 'head';
					// `#[serde(default)]` fills these as empty arrays for
					// sessions written by older builds (Phase 5.7).
					fs.reviewComments = folderSession.review_comments ?? [];
					fs.reviewedFiles = folderSession.reviewed_files ?? [];
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
		// Backend's active-folder pointer is now finalised. Let the
		// coder panel hydrate (sessions list + active session) for
		// the active folder; before this point a hydrate would have
		// raced the loop above and pulled the wrong folder's data
		// (e.g. surfaced moon-landing's session list while the
		// active folder was already moon-ide).
		coder.markWorkspaceReady();
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
	 * status. If the container isn't up yet (the backend is still
	 * `docker compose up`-ing the shell it auto-resumes on launch),
	 * defers the spawn until the `container:state` event fires
	 * `running` — up to a generous timeout (image pulls, slow
	 * `compose up --wait`). If the timeout fires, or the shell
	 * settles on a non-running state, falls back to host so the
	 * panel is never left without a terminal. Awaits
	 * `terminalRuntime` so the `terminal:output` listener is
	 * attached before we spawn — otherwise the first prompt bytes
	 * would be dropped on the floor.
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
		// Container isn't up yet — the launch-time auto-resume may
		// still be bringing it up. Wait for it rather than falling
		// back to host immediately; the `container:state` event
		// fires from `auto_resume_shell` when the shell settles.
		// Generous timeout: image pulls can take minutes, and a host
		// fallback after 60 s is better than none after 0 s.
		const started = await container.onceRunning(60_000);
		if (!started || bottomPanel.tabs.length > 0 || !bottomPanel.visible) {
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

	private gitAutoFetchWired = false;
	private gitAutoFetchInFlight = false;
	private gitAutoFetchLastAt = 0;

	/**
	 * Periodic `git fetch` against the active folder so the SCM
	 * panel's "Sync Changes" button surfaces when commits land
	 * upstream — `git_branch`'s ahead/behind read is local-ref-only
	 * and otherwise stays stale until the user manually pulls /
	 * pushes / runs `git fetch` from a terminal. Triggers:
	 *
	 * - **Once** ~5s after wire so the IDE has settled.
	 * - **Every 3 minutes** thereafter (matches VSCode / Cursor's
	 *   `git.autofetchPeriod` default — hardcoded; flip to a
	 *   setting when someone asks).
	 * - **On window focus**, throttled so an alt-tab flurry doesn't
	 *   spam fetches.
	 *
	 * Best-effort: failures (offline, auth refused, 30s timeout
	 * inside `git_fetch`) are silently swallowed — the user never
	 * asked us to fetch, so a flash toast would be noise. The
	 * backend's `tracing::debug!("git_fetch failed", ...)` in
	 * `run_git_fetch_quiet` is the supported triage channel
	 * (`RUST_LOG=moon_core=debug`). Skips when the document is
	 * hidden, when no folder is active, and when a fetch is
	 * already in flight.
	 */
	/**
	 * Subscribe to forwarded `$GIT_EDITOR` requests from in-container
	 * terminals. Pairs with `commands/editor_forward.rs` on the
	 * backend and the `moon-edit` shim in moon-base. See ADR 0021
	 * and `specs/containers.md` § "Editor forwarding".
	 *
	 * Idempotent — safe to call from `App.svelte`'s onMount even
	 * with HMR, same convention as the other `wire*` helpers.
	 */
	private editorForwardWired = false;
	private async wireEditorForward(): Promise<void> {
		if (this.editorForwardWired) {
			return;
		}
		this.editorForwardWired = true;
		try {
			await listen<EditRequest>('editor:request', (event) => {
				void this.handleEditorRequest(event.payload);
			});
		} catch {
			// Event bind failed — the `moon-edit` shim will time
			// out waiting on the parked socket, which will surface
			// as a `git commit` failure in the user's terminal.
			// Nothing actionable to surface here.
		}
	}

	private async handleEditorRequest(req: EditRequest) {
		// Open the file as a host-external buffer (LSP / git /
		// editorconfig skipped — appropriate for a commit message
		// or a rebase todo, neither of which we want indexed).
		// `openHostFile` is idempotent on path; if the buffer is
		// already open (rare but possible if the user re-amends),
		// it just focuses the existing tab.
		await this.openHostFile(req.host_path);
		// Tag the buffer with the parked-edit id. Lookup-by-path
		// because the array reference may have been replaced by
		// `openHostFile`'s reactive write.
		this.openFiles = this.openFiles.map((f) => (f.path === req.host_path ? { ...f, pendingEdit: req.id } : f));
		this.flash('Editing commit message — close the tab when done (right-click for Cancel).');
	}

	/**
	 * Resolve a forwarded edit by saving the buffer and replying
	 * `OK\n` on the parked socket so `git` proceeds. Called from
	 * the `Ctrl+S` save path when the active buffer carries a
	 * `pendingEdit`, and from a dedicated "Finish editing" tab
	 * affordance.
	 *
	 * Returns true when the edit was finished (the buffer existed
	 * and the backend acknowledged). Returns false when there's
	 * no longer a buffer to finish, in which case the caller
	 * should fall through to the regular save path.
	 */
	async finishPendingEdit(path: string): Promise<boolean> {
		const file = this.openFiles.find((f) => f.path === path);
		if (!file || file.pendingEdit === null) {
			return false;
		}
		const id = file.pendingEdit;
		try {
			await ipc.fs.writeFileHost(file.path, file.text);
		} catch (err) {
			this.flash(`Failed to save before finishing edit: ${formatError(err)}`);
			return true;
		}
		// Clear dirty + pending state first so a fast subsequent
		// close doesn't trigger the cancel-on-close path before the
		// backend's resolve has landed.
		this.openFiles = this.openFiles.map((f) =>
			f.path === path
				? {
						...f,
						isDirty: false,
						loadedFingerprint: fingerprint(f.text),
						pendingEdit: null,
					}
				: f,
		);
		try {
			await ipc.editorForward.finish(id);
		} catch (err) {
			// Backend resolve failed. The shim will probably time out
			// (which manifests as a `git commit` failure in the
			// terminal). Surface the error so the user knows to
			// retry from the terminal.
			this.flash(`Editor handoff failed: ${formatError(err)}`);
		}
		void this.closeFile(path);
		return true;
	}

	/**
	 * Cancel a forwarded edit by path: send `CANCEL\n` on the
	 * parked socket so `git` aborts, then close the tab. The
	 * default close-tab path finishes the edit (matches the
	 * "I'm done" muscle memory from every other editor); cancel
	 * is the explicit right-click affordance for the "actually,
	 * abort the commit" case.
	 */
	async cancelPendingEditForPath(path: string): Promise<void> {
		const file = this.openFiles.find((f) => f.path === path);
		if (!file || file.pendingEdit === null) {
			return;
		}
		const id = file.pendingEdit;
		// Clear `pendingEdit` first so the follow-up `closeFile`
		// doesn't recurse back through the "close = finish" hook
		// at the top of closeFile.
		this.openFiles = this.openFiles.map((f) => (f.path === path ? { ...f, pendingEdit: null } : f));
		try {
			await ipc.editorForward.cancel(id);
		} catch {
			// Best-effort — the shim will time out and `git` will
			// abort either way.
		}
		void this.closeFile(path);
	}

	private wireGitAutoFetch() {
		if (this.gitAutoFetchWired || typeof window === 'undefined') {
			return;
		}
		this.gitAutoFetchWired = true;
		window.setTimeout(() => void this.runGitAutoFetch('initial'), 5_000);
		window.setInterval(
			() => {
				void this.runGitAutoFetch('interval');
			},
			3 * 60 * 1000,
		);
		window.addEventListener('focus', () => {
			void this.runGitAutoFetch('focus');
		});
	}

	private async runGitAutoFetch(reason: 'initial' | 'interval' | 'focus' | 'folder-switch'): Promise<void> {
		if (this.gitAutoFetchInFlight) {
			return;
		}
		if (typeof document !== 'undefined' && document.visibilityState === 'hidden') {
			return;
		}
		if (!this.activeFolder) {
			return;
		}
		const now = Date.now();
		// 30s minimum between fetches except for the periodic timer.
		// Focus / folder-switch triggers are bursty by nature; the
		// 3-minute periodic tick is the floor for "we definitely
		// want a fresh fetch even if nothing else nudged us".
		if (reason !== 'interval' && now - this.gitAutoFetchLastAt < 30_000) {
			return;
		}
		this.gitAutoFetchLastAt = now;
		this.gitAutoFetchInFlight = true;
		// Snapshot HEAD before the fetch. A fetch itself only moves
		// remote-tracking refs — local HEAD/index/worktree are
		// unchanged — but the branch refresh we run afterwards also
		// picks up external ref moves in the cases the fs watcher
		// can't observe (it watches `.git/refs/` these days, but
		// inotify watch exhaustion, attach failures and network
		// mounts still leave blind spots). When the SHA differs
		// from the pre-fetch snapshot, kick a status refresh so the
		// SCM panel catches up instead of staying stale until the
		// next window-focus / manual refresh.
		const shaBefore = this.gitBranch.headShortSha;
		try {
			// Fetch failures (offline, no upstream, auth refused, 30s
			// timeout) are silently swallowed — the user never asked
			// us to fetch, so a flash toast / dev-console log would
			// be noise. The user-visible signal is "Sync Changes"
			// not appearing; the supported channel for triaging is
			// `RUST_LOG=moon_core=debug` plus the dev tools' Network
			// tab on the Tauri IPC.
			try {
				await ipc.fs.gitFetch();
			} catch {
				// Swallow — see above. Fall through to the branch
				// refresh: it's a local git probe (no network) and
				// still catches external ref moves the watcher can't.
			}
			// Fetch only moves remote-tracking refs; local working
			// tree, index, HEAD all unchanged. Just refresh the
			// branch readout so `behind` reflects the new upstream
			// and the Sync Changes button surfaces. A full
			// `refreshGitStatus` would be wasted work — unless HEAD
			// moved out from under us (see `shaBefore` above).
			await this.refreshGitBranch();
			if (this.gitBranch.headShortSha !== null && this.gitBranch.headShortSha !== shaBefore) {
				frontendLog(
					'fs-watcher',
					'info',
					`HEAD moved (${shaBefore} → ${this.gitBranch.headShortSha}) during auto-fetch — refreshing git status`,
				);
				void this.refreshGitStatus(this.paths, null);
			}
		} catch {
			// `refreshGitBranch` itself is best-effort and shouldn't
			// throw (it collapses to a null state), but guard anyway.
		} finally {
			this.gitAutoFetchInFlight = false;
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
			const isPersistable = (p: string) => !isSyntheticBufferPath(p) && !externalSet.has(p);
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
						pr_scope: fs.prScope,
						compare_baseline: fs.compareBaseline,
						review_comments: fs.reviewComments,
						reviewed_files: fs.reviewedFiles,
						// Persist how the folder was bound so a worktree
						// (ADR 0028) re-binds as a worktree on next launch
						// instead of becoming a plain top-level folder.
						origin: folder.origin,
					});
				}
			}
			const session: WorkspaceSession = {
				folders: folderSessions,
				active_folder_path: ws?.active_folder ?? null,
			};
			// `workspaces`, `slack`, `right_panel`, and `coder` are
			// written through their own paths (Phase 7.2 bootstrap
			// + Phase 7.6 IPC for the workspace catalog; `slack_*`,
			// `ui_set_right_panel`, `coder_*` Tauri commands for
			// the rest). The backend's `app_state_save` ignores
			// whatever we send for those fields and preserves the
			// on-disk value. The placeholders satisfy the shared
			// type only.
			const payload: AppState = {
				workspaces: [],
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
			// AppState + session writes are best-effort. A toast on
			// every failure would be too noisy (this fires on every
			// navigation); a global frontend logger doesn't exist
			// yet (and isn't worth adding for one callsite). If
			// saves systematically fail the next launch's restore
			// will simply have no data — that's loud enough. The
			// two writes are issued in parallel rather than chained
			// so a slow save on one path doesn't gate the other.
			void ipc.appState.save(payload).catch(() => {});
			if (ws) {
				void ipc.session.save(session).catch(() => {});
			}
		});
	}

	/**
	 * Monotonic token identifying the most recent `loadPaths` call.
	 * A walk that comes back with a stale token was superseded
	 * mid-flight — typically by a folder switch firing its own
	 * walk — and must drop its result: the `paths` setter resolves
	 * `activeFolderState` at assignment time, so a late walk from
	 * folder A would otherwise write A's path list into folder B's
	 * state.
	 */
	#loadPathsToken = 0;

	async loadPaths(changedSubset: ReadonlySet<string> | null = null) {
		if (!this.activeFolder) {
			return;
		}
		const token = ++this.#loadPathsToken;
		this.loadingPaths = true;
		// Profiling: see `setActiveFolder` above for the wider
		// timeline. The `walk` measure is the recursive backend
		// walk + IPC; `assign` is the synchronous reactive
		// assignment plus whatever effects Svelte flushes
		// synchronously in response (FileTree's path-set effect
		// runs here, calling `tree.resetPaths(merged)`).
		performance.mark('moon:loadPaths.start');
		const tStart = performance.now();
		try {
			// One IPC call, full recursive walk backend-side. The
			// previous implementation fired one `readDir` per
			// directory which at Tauri's per-call framing cost
			// dominated refresh latency (the walk itself is a
			// sub-hundred-ms `read_dir` storm).
			const collected = await ipc.fs.collectPaths(MAX_TREE_DEPTH);
			if (token !== this.#loadPathsToken) {
				// Superseded while on the wire — a newer walk owns
				// the UI (and possibly a different active folder).
				return;
			}
			const tWalk = performance.now();
			performance.measure('moon:loadPaths.walk', 'moon:loadPaths.start');
			performance.mark('moon:loadPaths.assign.start');
			this.paths = collected.paths;
			this.depthCappedPaths = collected.depth_capped;
			const tAssign = performance.now();
			performance.measure('moon:loadPaths.assign', 'moon:loadPaths.assign.start');
			// Classify git status in the background — the tree can
			// paint before we know the answer. Pierre reconciles
			// `setGitStatus` updates in place, so late-arriving
			// entries fade / tint the affected rows without a reflow.
			void this.refreshGitStatus(collected.paths, changedSubset);
			// eslint-disable-next-line no-console
			console.info(
				`moon-ide: loadPaths(${this.activeFolderPath}) ` +
					`walk=${(tWalk - tStart).toFixed(1)}ms ` +
					`assign=${(tAssign - tWalk).toFixed(1)}ms ` +
					`count=${collected.paths.length} ` +
					`depthCapped=${collected.depth_capped.length}`,
			);
		} catch (err) {
			this.flash(`Failed to read folder: ${formatError(err)}`);
		} finally {
			// Only the latest call may clear the flag — a superseded
			// walk resolving late must not hide the spinner (and
			// re-open the `refreshActiveFolder` coalescing gate)
			// while the newer walk is still in flight.
			if (token === this.#loadPathsToken) {
				this.loadingPaths = false;
			}
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
	 * Per-folder aggregate change counts driving the project-bar
	 * badges. Keyed by absolute folder path. Refreshed on workspace
	 * hydration, on add/remove, and on every active-folder
	 * `refreshGitStatus` pass — that last one is the load-bearing
	 * trigger for the "agent in folder A modified folder B" case:
	 * the watcher fires on A's tree, the active refresh fans out,
	 * and B's badge updates without B ever becoming active.
	 *
	 * Folders that aren't repos / have no changes resolve to a
	 * zeroed `GitChangeSummary`; missing entries (folder freshly
	 * added, refresh in flight) render as no badges either.
	 */
	gitChangeSummaries = new SvelteMap<string, GitChangeSummary>();

	/**
	 * In-flight guard so a watcher burst (the agent saving 30 files
	 * back-to-back) doesn't stack 30 `git status` subprocesses per
	 * folder. The first call wins; subsequent ones short-circuit
	 * until the in-flight one resolves and clears the flag.
	 */
	#summaryInFlight = new Set<string>();

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
		upstreamTracked: false,
		upstreamForeign: false,
		ahead: 0,
		behind: 0,
		prUrl: null,
		defaultBranchRemoteRef: null,
		defaultBranchBehind: 0,
		previousBranch: null,
	});

	/**
	 * Snapshot of the active folder's in-flight merge. The SCM
	 * panel reshapes itself (Merging banner above the composer,
	 * Commit merge / Abort merge buttons, hidden sync controls)
	 * when `inProgress` is true. Mirrors
	 * `moon_protocol::git::GitMergeState`.
	 *
	 * Refreshed:
	 *   - on folder switch alongside `refreshGitBranch`,
	 *   - after every git op that could change merge state
	 *     (commit, restore, merge attempt, abort),
	 *   - from the fs-watcher when a `.git/` write lands —
	 *     `MERGE_HEAD` / `MERGE_MSG` creation and removal both
	 *     trigger one, so the panel feels live without a poll.
	 *
	 * Failures collapse to the default (no merge in progress) —
	 * a transient probe glitch should never strand the panel in
	 * a stale "Merging…" state.
	 */
	gitMergeState = $state<GitMergeState>({
		inProgress: false,
		mergingRef: null,
		defaultMessage: null,
		unmergedPaths: [],
	});

	/**
	 * Recent commits on the active folder's current branch. The SCM
	 * panel renders these below the sync buttons — a compact list
	 * (short SHA, subject, relative date) so the user always has a
	 * glance at what just landed, especially when the working tree
	 * is clean and the rest of the panel feels empty. Refreshed on
	 * folder switch, after commits, and on every `refreshGitStatus`
	 * pass so external commits surface without a manual refresh.
	 */
	gitCommits = $state<readonly CommitEntry[]>([]);

	/**
	 * URL of the open GitHub PR matching the active folder's
	 * current branch, or `null` when there isn't one (or `gh`
	 * isn't installed / authed, the remote isn't GitHub, etc.).
	 * Refreshed via `gh pr list --head <branch> --limit 1` — a
	 * network call — so we only re-query when `gitBranch.name`
	 * actually changes, plus once after a successful push /
	 * publish / pull (a PR may have appeared or been closed
	 * externally and a manual sync is the natural moment to find
	 * out). The SCM panel uses this to retarget the "Open PR"
	 * button at the existing PR instead of the create-PR URL when
	 * one exists.
	 */
	gitExistingPrUrl = $state<string | null>(null);

	/**
	 * Branch name we last queried `gitExistingPrUrl` for. Used to
	 * skip the network call when the branch hasn't changed since
	 * the last refresh — `refreshGitBranch` runs on a fast cadence
	 * (auto-fetch tick, every status pass) and we don't want to
	 * spawn `gh` that often.
	 */
	private gitExistingPrUrlBranch: string | null = null;
	private gitExistingPrUrlInFlight = false;

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

	/**
	 * Pull the active folder's branch name, upstream config, and
	 * ahead/behind counters from the backend. Public so the SCM
	 * panel can await it after `sync()` / `publish()` to keep the
	 * sync button disabled until the counters drop to zero —
	 * without this the button briefly un-disables before
	 * unmounting because the inner pull/push call returns before
	 * the next branch refresh tick lands.
	 *
	 * Failures collapse to a "no branch info" state; we'd rather
	 * the SCM panel render a neutral header than surface a flash
	 * for a transient git probe failure.
	 */
	async refreshGitBranch() {
		try {
			this.gitBranch = await ipc.fs.gitBranch();
		} catch {
			this.gitBranch = {
				name: null,
				headShortSha: null,
				hasUpstream: false,
				upstreamTracked: false,
				upstreamForeign: false,
				ahead: 0,
				behind: 0,
				prUrl: null,
				defaultBranchRemoteRef: null,
				defaultBranchBehind: 0,
				previousBranch: null,
			};
		}
		// Merge state is a sibling probe — `.git/MERGE_HEAD`
		// exists exactly while the working tree is mid-merge, and
		// the branch refresh is the natural moment to fan a fresh
		// snapshot to the panel. Cheap (small fs reads + one
		// `ls-files --unmerged`) and idempotent.
		void this.refreshGitMergeState();
		// Branch may have changed under us (external `git switch` /
		// `gh pr checkout` from a terminal, or our own
		// `switchToBranch`). Refresh the existing-PR pointer
		// opportunistically when that happens; the network call
		// short-circuits to `null` for non-GitHub / detached /
		// gh-missing cases and stays cheap to skip.
		void this.refreshGitExistingPrUrl({ force: false });
		// Recent commits list is a sibling probe — a commit just
		// landed (ours or external), so the panel's list should
		// reflect the new tip. Best-effort: errors collapse to an
		// empty list rather than a flash.
		void this.refreshGitLog();
	}

	/**
	 * Refresh `gitCommits` from `git log`. The SCM panel renders the
	 * list below the sync buttons; a glance at recent commits is
	 * especially useful when the working tree is clean. Failures
	 * collapse to an empty list — a transient git glitch shouldn't
	 * strand the panel with stale commits.
	 */
	async refreshGitLog() {
		try {
			this.gitCommits = await ipc.fs.gitLog();
		} catch {
			this.gitCommits = [];
		}
	}

	/**
	 * Refresh `gitExistingPrUrl` from `gh pr list --head <branch>`.
	 * Skipped (no-op) when the branch hasn't changed since the
	 * last query unless `force` is set — push / publish / pull are
	 * the gestures that earn a forced refresh, since a PR may
	 * have been opened or closed by the same action.
	 *
	 * Errors collapse to `null`; this is a "give me a URL if you
	 * have one" call, never load-bearing.
	 */
	async refreshGitExistingPrUrl({ force }: { force: boolean }) {
		const branch = this.gitBranch.name;
		if (branch === null) {
			this.gitExistingPrUrl = null;
			this.gitExistingPrUrlBranch = null;
			return;
		}
		if (!force && branch === this.gitExistingPrUrlBranch) {
			return;
		}
		if (this.gitExistingPrUrlInFlight) {
			return;
		}
		this.gitExistingPrUrlInFlight = true;
		try {
			const url = await ipc.fs.gitExistingPrUrl();
			// Branch may have flipped while gh was running; only
			// commit the result when it still matches the branch
			// we queried for (the next `refreshGitBranch` will
			// reschedule otherwise).
			if (this.gitBranch.name === branch) {
				this.gitExistingPrUrl = url;
				this.gitExistingPrUrlBranch = branch;
			}
		} catch {
			if (this.gitBranch.name === branch) {
				this.gitExistingPrUrl = null;
				this.gitExistingPrUrlBranch = branch;
			}
		} finally {
			this.gitExistingPrUrlInFlight = false;
		}
	}

	/**
	 * Reactive lookup for the project bar. `undefined` until the
	 * folder's first refresh resolves; consumers render no badges
	 * in that case.
	 */
	gitChangeSummaryFor(folderPath: string): GitChangeSummary | undefined {
		return this.gitChangeSummaries.get(folderPath);
	}

	/**
	 * Pull the change summary for a single folder. Coalesces with
	 * any in-flight refresh for the same folder. Failures cache as
	 * a zeroed summary — the most common reason is "git unavailable
	 * / not a repo / folder vanished", and rendering "no badges"
	 * is the right answer for all of them.
	 */
	async refreshGitChangeSummary(folderPath: string): Promise<void> {
		if (this.#summaryInFlight.has(folderPath)) {
			return;
		}
		this.#summaryInFlight.add(folderPath);
		try {
			const summary = await ipc.fs.gitChangeSummary(folderPath);
			this.gitChangeSummaries.set(folderPath, summary);
		} catch {
			this.gitChangeSummaries.set(folderPath, { added: 0, modified: 0, deleted: 0 });
		} finally {
			this.#summaryInFlight.delete(folderPath);
		}
	}

	/**
	 * Fan out a summary refresh to every bound folder. Cheap when
	 * folders are small; the per-folder in-flight guard collapses
	 * bursts. Used after workspace hydration and on every active-
	 * folder `refreshGitStatus` pass so cross-folder edits (an
	 * agent in folder A modifying folder B) reach the project bar
	 * without B becoming active.
	 */
	async refreshAllGitChangeSummaries(): Promise<void> {
		const folders = this.workspace?.folders ?? [];
		await Promise.all(folders.map((f) => this.refreshGitChangeSummary(f.path)));
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
			// "Stage everything then commit" clears the working tree,
			// so the SCM filter has nothing left to show — flip it
			// off so the file tree snaps back to the regular full
			// view instead of stranding the user on an empty
			// changes-only pane with the filter pill still lit.
			this.scmFilterOn = false;
			await this.refreshGitBranch();
			void this.refreshActiveFolder();
			// Tie the visible coder session to the branch this commit
			// landed on (ADR 0028), so the session list can offer a
			// one-click switch back. Best-effort; no-op without a
			// persisted session open.
			void coder.associateActiveSessionBranch();
			return true;
		} catch (err) {
			this.flash(`Commit failed: ${formatError(err)}`);
			return false;
		}
	}

	/**
	 * Create a fresh branch from `HEAD`, switch to it, then commit
	 * the working-tree + staged changes with `message`. Used by
	 * the SCM panel's "Commit to new branch…" inline form. Branch
	 * validation, conflict checks, and rollback-on-failure all
	 * live server-side in `git_commit_on_new_branch`; we just
	 * surface success / failure via the same flash + refresh
	 * pattern as `commitChanges`.
	 */
	/**
	 * Keep a worktree folder's bar branch label in sync with its
	 * actual checked-out branch (ADR 0028). The label comes from the
	 * registry's `FolderOrigin::Worktree { branch }`, stamped at
	 * creation; an in-worktree commit-on-new-branch or `git switch`
	 * changes the real branch, so this re-stamps it and re-adopts the
	 * snapshot. No-op when the active folder isn't a worktree.
	 */
	private async syncWorktreeBranchLabel(): Promise<void> {
		if (this.activeFolder?.origin.kind !== 'worktree') {
			return;
		}
		try {
			const ws = await ipc.workspace.syncActiveWorktreeBranch();
			await this.adoptWorkspaceSnapshot(ws);
		} catch (err) {
			frontendLog('workspace', 'warn', `worktree branch label sync failed: ${formatError(err)}`);
		}
	}

	async commitChangesOnNewBranch(branch: string, message: string) {
		const trimmedBranch = branch.trim();
		const trimmedMessage = message.trim();
		if (trimmedBranch.length === 0) {
			this.flash('Branch name is empty.');
			return false;
		}
		if (trimmedMessage.length === 0) {
			this.flash('Commit message is empty.');
			return false;
		}
		try {
			const result = await ipc.fs.gitCommitOnNewBranch(trimmedBranch, trimmedMessage);
			this.flash(`Committed ${result.shortSha} on ${trimmedBranch}: ${result.summary}`);
			// Same reasoning as `commitChanges`: post-commit working
			// tree is empty, so the SCM filter has nothing to filter
			// to — flip it off rather than stranding the user on an
			// empty changes-only pane.
			this.scmFilterOn = false;
			await this.refreshGitBranch();
			void this.refreshActiveFolder();
			// Same as `commitChanges`: tie the visible session to the
			// branch we just landed on — here the freshly-created one
			// (ADR 0028).
			void coder.associateActiveSessionBranch();
			// If this happened inside a worktree, the new branch is now
			// its checked-out branch — re-stamp the folder bar label.
			void this.syncWorktreeBranchLabel();
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
			// A push is the natural moment to check for a newly-
			// opened PR (someone may have created one on github.com
			// while we were ahead-but-not-yet-pushed, or our push
			// itself unblocks an existing draft).
			void this.refreshGitExistingPrUrl({ force: true });
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
			// A freshly-published branch usually doesn't have a PR
			// yet, but `gh pr create --fill` from a terminal could
			// have produced one in parallel — same reasoning as
			// `pushChanges`. Force the refresh so we don't keep
			// pointing at the create-PR URL after the user already
			// opened one out-of-band.
			void this.refreshGitExistingPrUrl({ force: true });
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

	/**
	 * `git merge --no-edit <remoteRef>` for the active folder.
	 * Backed by whatever `gitBranch.defaultBranchRemoteRef` was at
	 * call time (the SCM panel passes it in so we don't risk
	 * merging a stale default that drifted between the click and
	 * the IPC). Conflicts and dirty-tree refusals propagate via
	 * flash with git's stderr verbatim. On success we refresh the
	 * full active folder so newly-merged files light up the tree
	 * the same way `pullChanges` does.
	 */
	async mergeDefaultBranch(remoteRef: string) {
		try {
			await ipc.fs.gitMergeDefaultBranch(remoteRef);
			const shortName = remoteRef.split('/').slice(1).join('/') || remoteRef;
			this.flash(`Merged ${shortName} into current branch.`);
			void this.refreshActiveFolder();
			return true;
		} catch (err) {
			// Merge conflicts and dirty-tree refusals both land
			// here. The post-call refresh below picks up the
			// `.git/MERGE_HEAD` write so the SCM panel shifts
			// into merge-in-progress mode without an extra
			// click — even on the conflict path we want the
			// user to land on a panel that's ready to drive
			// the resolution flow, not a flash they have to
			// dismiss before they can see what happened.
			this.flash(`Merge failed: ${formatError(err)}`);
			void this.refreshActiveFolder();
			return false;
		}
	}

	/**
	 * Refresh `gitMergeState` from `git_merge_state`. Best-effort:
	 * a probe failure collapses to "no merge in progress" rather
	 * than stranding the panel on a stale snapshot. Awaited by
	 * the panel's mount + post-op refresh paths so the next
	 * render reflects the new state; called fire-and-forget by
	 * the fs-watcher `.git/` branch.
	 */
	async refreshGitMergeState() {
		try {
			this.gitMergeState = await ipc.fs.gitMergeState();
		} catch {
			this.gitMergeState = {
				inProgress: false,
				mergingRef: null,
				defaultMessage: null,
				unmergedPaths: [],
			};
		}
	}

	/**
	 * Finish an in-progress merge by running the regular
	 * `git_commit` path with whatever bytes are in the composer.
	 * Git resolves the rest: with `.git/MERGE_HEAD` present, the
	 * commit it produces is a merge commit (two parents); the
	 * unmerged-path gate is enforced server-side too. On success
	 * we refresh both `gitBranch` and `gitMergeState` so the
	 * panel reverts to its regular shape — the post-merge
	 * commit clears `MERGE_HEAD`.
	 *
	 * Soft-warn first: scan every previously-conflicted file's
	 * on-disk bytes for leftover marker lines. The unmerged
	 * index can be empty (`git add` was run, or there were no
	 * content conflicts) while a file still has `<<<<<<<` in
	 * it — the staged version went into the index without the
	 * markers being touched. Committing that is almost never
	 * what the user meant, so we prompt for confirm before
	 * letting git produce the merge commit.
	 */
	async commitMerge(message: string) {
		const suspicious = await this.findLeftoverConflictMarkerFiles();
		if (suspicious.length > 0) {
			const ok = await confirm(
				`The following file${suspicious.length === 1 ? '' : 's'} still contain${suspicious.length === 1 ? 's' : ''} merge-conflict markers:\n\n${suspicious.join('\n')}\n\nCommit anyway?`,
				{ title: 'Commit merge?', kind: 'warning' },
			);
			if (!ok) {
				return false;
			}
		}
		try {
			const result = await ipc.fs.gitCommit(message, false);
			this.flash(`Committed ${result.shortSha}: ${result.summary}`);
			this.scmFilterOn = false;
			// Refresh `gitMergeState` *before* returning so the SCM
			// panel's merge-prefill `$effect` sees `inProgress: false`
			// the moment the local handler clears `commitDraft`. The
			// fs-watcher would refresh us too once `.git/MERGE_HEAD`
			// disappears, but on that path there's a window where the
			// effect re-runs with stale `inProgress: true` +
			// `defaultMessage`, restamping `MERGE_MSG` back into the
			// composer the user just submitted.
			await Promise.all([this.refreshGitBranch(), this.refreshGitMergeState()]);
			void this.refreshActiveFolder();
			return true;
		} catch (err) {
			this.flash(`Commit merge failed: ${formatError(err)}`);
			return false;
		}
	}

	/**
	 * Sniff the working tree for files that still carry
	 * `<<<<<<<` / `=======` / `>>>>>>>` lines despite git
	 * reporting them as resolved (no longer in
	 * `git ls-files --unmerged`). Best-effort: a read failure
	 * for any one path drops that path from the result without
	 * blocking the commit — git's own `nothing-to-commit` gate
	 * is the hard backstop.
	 *
	 * We scan only paths git currently reports as `modified` /
	 * `added` / `conflicted` (the rows that could conceivably
	 * carry leftover markers). Untracked files are excluded —
	 * `git commit` with `MERGE_HEAD` present wouldn't pick them
	 * up anyway; the user would notice on the next normal
	 * commit.
	 */
	async findLeftoverConflictMarkerFiles(): Promise<string[]> {
		const candidates = new Set<string>();
		for (const entry of this.gitStatusEntries) {
			if (entry.status === 'modified' || entry.status === 'added' || entry.status === 'conflicted') {
				candidates.add(entry.path);
			}
		}
		if (candidates.size === 0) {
			return [];
		}
		const out: string[] = [];
		await Promise.all(
			[...candidates].map(async (path) => {
				try {
					const res = await ipc.fs.readFile(path);
					if (!res.is_binary && hasConflictMarkerLines(res.text)) {
						out.push(path);
					}
				} catch {
					// Read failure (file vanished between status
					// and probe, etc.) — drop silently and let the
					// commit proceed without flagging this row.
				}
			}),
		);
		return out.toSorted();
	}

	/**
	 * `git merge --abort` — wind back the merge so the working
	 * tree returns to the pre-merge HEAD. The flash uses git's
	 * stderr verbatim for the failure case (no merge to abort,
	 * dirty pre-merge state); success quietly refreshes the
	 * panel and the tree.
	 */
	async abortMerge() {
		try {
			await ipc.fs.gitMergeAbort();
			this.flash('Merge aborted.');
			// Refresh `gitMergeState` synchronously alongside the
			// branch so the panel's merge-prefill `$effect` sees
			// `inProgress: false` before the local handler touches
			// `commitDraft`. Same race as `commitMerge`.
			await Promise.all([this.refreshGitBranch(), this.refreshGitMergeState()]);
			void this.refreshActiveFolder();
			return true;
		} catch (err) {
			this.flash(`Abort failed: ${formatError(err)}`);
			return false;
		}
	}

	/**
	 * Branch-switcher palette state. `open` flips on via
	 * `openBranchSwitcher()` (Cmd+Shift+B / click on the branch
	 * label) and off via `closeBranchSwitcher()`. The list is
	 * fetched lazily on open so we don't pay the
	 * `git for-each-ref` + `gh pr list` round-trip until the user
	 * actually asks. `loading` is true during the fetch; rows
	 * paint as soon as `list` is set.
	 *
	 * `list` defaults to a "no rows yet, treat the PR section as
	 * unavailable" stub so the UI can render an empty state on
	 * first paint without dealing with `null`.
	 */
	branchSwitcher = $state<{
		open: boolean;
		loading: boolean;
		switching: boolean;
		list: BranchList;
	}>({
		open: false,
		loading: false,
		switching: false,
		list: { local: [], prs: [], prStatus: { kind: 'ok' } },
	});

	/**
	 * PR-section filter for the *active* folder, surfaced as a
	 * derived alias so the palette can read/write
	 * `workspace.prScope` without reaching into `FolderState`.
	 * Falls back to `'all'` when no folder is bound — the
	 * palette never opens in that state, but the typing keeps
	 * the toggle's `disabled` branch trivial.
	 */
	get prScope(): PrListScope {
		const active = this.workspace?.active_folder ?? null;
		const fs = active === null ? null : this.folderStates.get(active);
		return fs?.prScope ?? 'all';
	}

	openBranchSwitcher() {
		if (this.branchSwitcher.open) {
			return;
		}
		this.branchSwitcher.open = true;
		void this.refreshBranchList();
	}

	closeBranchSwitcher() {
		this.branchSwitcher.open = false;
		this.branchSwitcher.switching = false;
	}

	setPrScope(scope: PrListScope) {
		const active = this.workspace?.active_folder ?? null;
		const fs = active === null ? null : this.folderStates.get(active);
		if (!fs || fs.prScope === scope) {
			return;
		}
		fs.prScope = scope;
		// Persist the new pref alongside other folder state. The
		// schedule is debounced inside `persistAppState`, so
		// flipping the toggle once is one disk write.
		this.persistAppState();
		void this.refreshBranchList();
	}

	/**
	 * Active folder's SCM compare baseline, surfaced as a derived
	 * alias so the SCM panel can read/write `workspace.compareBaseline`
	 * without reaching into `FolderState`. Falls back to `'head'`
	 * when no folder is bound.
	 */
	get compareBaseline(): CompareBaseline {
		const active = this.workspace?.active_folder ?? null;
		const fs = active === null ? null : this.folderStates.get(active);
		return fs?.compareBaseline ?? 'head';
	}

	/**
	 * SHA of the merge-base with the default branch, when the
	 * `'default'` baseline applies. `null` outside the active
	 * folder, when the host returned `None` (no default branch /
	 * detached / on default branch / no merge-base), or when the
	 * baseline is `'head'`.
	 */
	get defaultBranchMergeBase(): string | null {
		const active = this.workspace?.active_folder ?? null;
		const fs = active === null ? null : this.folderStates.get(active);
		return fs?.defaultBranchMergeBase ?? null;
	}

	/**
	 * Short label for the default branch the SCM toggle compares
	 * against — e.g. `'origin/main'`. `null` when no default branch
	 * could be resolved; the SCM panel renders the toggle in
	 * disabled state in that case.
	 */
	get defaultBranchName(): string | null {
		const active = this.workspace?.active_folder ?? null;
		const fs = active === null ? null : this.folderStates.get(active);
		return fs?.defaultBranchName ?? null;
	}

	/**
	 * Flip the active folder's SCM compare baseline. Same
	 * persistence + debounce contract as `setPrScope`. Triggers
	 * a status refresh so the file tree, gutter, and diff view
	 * pick up the new source on the next paint.
	 *
	 * Setting `'default'` is sticky even when the host can't
	 * resolve a default branch (e.g. fresh repo before the first
	 * push): the toggle stays in `'default'` so flipping back
	 * later doesn't require remembering. Every consumer reads
	 * `defaultBranchMergeBase`; `null` there means
	 * "fall back to HEAD-blob semantics for this paint".
	 */
	setCompareBaseline(baseline: CompareBaseline) {
		const active = this.workspace?.active_folder ?? null;
		const fs = active === null ? null : this.folderStates.get(active);
		if (!fs || fs.compareBaseline === baseline) {
			return;
		}
		fs.compareBaseline = baseline;
		// Drop cached blobs for the old baseline — same path
		// might map to a different `git show <rev>:<path>`
		// result under the new baseline. Re-fetched lazily on
		// the next `refreshHead` for each open buffer.
		this.headByPath = new Map();
		this.persistAppState();
		void this.refreshGitStatus(this.paths, null);
	}

	async refreshBranchList() {
		this.branchSwitcher.loading = true;
		try {
			this.branchSwitcher.list = await ipc.fs.branchList(this.prScope);
		} catch (err) {
			this.flash(`Branch list failed: ${formatError(err)}`);
			this.branchSwitcher.list = {
				local: [],
				prs: [],
				prStatus: { kind: 'failed', detail: formatError(err) },
			};
		} finally {
			this.branchSwitcher.loading = false;
		}
	}

	/**
	 * Switch the active folder to `target`. Closes the palette
	 * on success and refreshes the active folder so the file
	 * tree, branch label, and SCM panel pick up the new HEAD.
	 * Failures (dirty tree, missing branch, gh auth required)
	 * propagate as a flash with git / gh's stderr verbatim and
	 * leave the palette open so the user can pick a different
	 * row.
	 */
	async switchToBranch(target: BranchSwitchTarget): Promise<boolean> {
		if (this.branchSwitcher.switching) {
			return false;
		}
		this.branchSwitcher.switching = true;
		try {
			await ipc.fs.branchSwitch(target);
			const label = target.kind === 'local' ? target.name : `PR #${target.number}`;
			this.flash(`Switched to ${label}.`);
			this.closeBranchSwitcher();
			await this.refreshGitBranch();
			void this.refreshActiveFolder();
			// Switching a worktree's branch changes its bar label too.
			void this.syncWorktreeBranchLabel();
			return true;
		} catch (err) {
			this.flash(`Switch failed: ${formatError(err)}`);
			return false;
		} finally {
			this.branchSwitcher.switching = false;
		}
	}

	private async refreshGitStatus(paths: readonly string[], changedSubset: ReadonlySet<string> | null) {
		// Refresh the branch label opportunistically alongside the
		// status fetch — `git symbolic-ref` is cheap and we want the
		// SCM panel header to update when external `git checkout` /
		// `git switch` runs from a terminal. The fs-watcher forwards
		// `.git/HEAD` writes and ref moves under `.git/refs/`
		// (commit / push / fetch), so this opportunistic refresh is
		// belt-and-braces for the cases inotify can't observe (watch
		// limit exhaustion, network mounts) plus the ahead/behind
		// counts, which change with the status anyway.
		void this.refreshGitBranch();
		// `'default'` baseline routes through `git diff
		// --name-status` against the merge-base; the entries
		// already carry the right modified/added/deleted labels
		// for the file tree. When the host can't compute that
		// diff (no default branch, on default branch, detached
		// HEAD, no merge-base) we fall back to the regular
		// `git status` path so the tree still paints something
		// sensible — the toggle stays sticky for free.
		const fs =
			this.workspace?.active_folder !== null && this.workspace !== null
				? (this.folderStates.get(this.workspace.active_folder ?? '') ?? null)
				: null;
		const useDefaultBaseline = fs?.compareBaseline === 'default';
		let usedDefaultBaseline = false;
		if (useDefaultBaseline) {
			try {
				const diff = await ipc.fs.gitDefaultBranchDiff();
				if (diff !== null) {
					this.gitStatusEntries = diff.entries;
					if (fs !== null) {
						fs.defaultBranchMergeBase = diff.mergeBase;
						fs.defaultBranchName = diff.defaultBranchRef;
					}
					usedDefaultBaseline = true;
				} else if (fs !== null) {
					fs.defaultBranchMergeBase = null;
					fs.defaultBranchName = null;
				}
			} catch {
				// Same fallback discipline as `gitStatusEntries`
				// below — silently degrade rather than toasting
				// a tree-cosmetics error. The branch-vs-main
				// view appears empty until the next refresh.
				if (fs !== null) {
					fs.defaultBranchMergeBase = null;
					fs.defaultBranchName = null;
				}
			}
		} else if (fs !== null) {
			fs.defaultBranchMergeBase = null;
			fs.defaultBranchName = null;
		}
		if (!usedDefaultBaseline) {
			try {
				this.gitStatusEntries = await ipc.fs.gitStatusEntries([...paths]);
			} catch {
				// Non-fatal — we'd rather leave the tree untinted
				// than noise up the toast for a git probe
				// failure. If git is absent the command still
				// succeeds (returns []), so throwing here is a
				// legitimate filesystem error worth ignoring for
				// tree cosmetics.
				this.gitStatusEntries = [];
			}
		}
		// Clear any reviewed-file marks whose file changed since it
		// was ticked (a new commit / pull / local edit). Fire-and-
		// forget — it only resolves SHAs for the few marked files and
		// drops stale ones; the review tab's "Viewed" checkboxes
		// repaint reactively.
		void this.refreshReviewedFiles();
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
		// A write to `.git/HEAD` (external `git switch` / `git
		// checkout <branch>`) moves the repo's history pointer
		// without necessarily touching a working-tree file: when the
		// new branch's content matches the old, no per-file event
		// fires. The fs-watcher forwards those `.git/` top-level
		// writes (HEAD / MERGE_HEAD / MERGE_MSG) precisely so we can
		// treat them as "history moved" and re-attribute open
		// buffers' blame — otherwise the inline widget keeps showing
		// the previous branch's authorship. We only re-blame buffers
		// *not* in `changedSubset`: a switch that did change a file's
		// bytes also fired that file's own working-tree event, so it
		// reloads + re-blames through the branch below. Splitting it
		// this way means no buffer is blamed twice in one pass.
		const gitStateMoved = changedSubset !== null && subsetTouchesGitState(changedSubset);
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
			const inSubset = changedSubset === null || changedSubset.has(file.path);
			if (!inSubset && !gitStateMoved) {
				frontendLog(
					'fs-watcher',
					'debug',
					`reload skipped: open file '${file.path}' not in subset of ${changedSubset?.size ?? 0} path(s)`,
				);
				continue;
			}
			this.refreshHead(file.path);
			if (!inSubset) {
				// History moved (branch switch / merge) but this
				// file's bytes didn't change on disk, so there's
				// nothing to reload — only the per-line attribution
				// can differ across the two histories. Re-blame
				// without touching the buffer.
				frontendLog('fs-watcher', 'info', `re-blaming '${file.path}' after git state change (no working-tree edit)`);
				this.scheduleBlameRefresh(file.path);
				continue;
			}
			if (file.isDirty) {
				frontendLog(
					'fs-watcher',
					'info',
					`reload skipped: '${file.path}' is dirty (unsaved edits — would clobber); HEAD refresh only`,
				);
			} else {
				frontendLog(
					'fs-watcher',
					'info',
					`reloading '${file.path}' from disk (matched fs:changed${changedSubset === null ? ' [null subset → full sweep]' : ''})`,
				);
				void this.reloadOpenFileFromDisk(file.path);
				// On-disk content changed → `git blame` may now
				// report different commits (a fresh commit, a branch
				// switch that rewrote these lines, a working-tree
				// edit showing "Not Committed Yet"). The in-memory
				// buffer won't re-attribute itself; kick a debounced
				// refresh so the inline widget catches up. The
				// per-path timer in `scheduleBlameRefresh` collapses
				// this with any other trigger in the same burst.
				this.scheduleBlameRefresh(file.path);
			}
		}
		// Fan out to every bound folder's project-bar badge. The
		// active folder's status was just (re)classified above; for
		// inactive folders we issue a separate `git status` so an
		// agent in folder A modifying folder B reaches B's badge
		// even though B isn't being watched. Cheap: per-folder
		// `git status` is sub-100ms on real repos and the
		// in-flight guard collapses bursts.
		void this.refreshAllGitChangeSummaries();
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
	 * Coalesces with an in-flight walk: alt-tab flurry doesn't
	 * stack two concurrent recursions. **But** the coalescing
	 * accumulates the changed subset across calls — silently
	 * dropping a second event's subset is the bug that made
	 * "switch branch in terminal" not refresh open files when the
	 * checkout's topology event kicked off a long walk and the
	 * working-tree writes for the open file landed in a second
	 * `fs:changed` burst that arrived mid-walk. After the
	 * in-flight refresh finishes we drain the pending subset into
	 * one follow-up `refreshActiveFolder` call so every observed
	 * path eventually flows through the per-buffer reload loop.
	 */
	#pendingRefreshSubset: Set<string> | null = null;
	#pendingRefreshNullSubset = false;
	#pendingRefreshTopology = false;

	async refreshActiveFolder(changedSubset: ReadonlySet<string> | null = null, topologyChanged = true): Promise<void> {
		if (!this.activeFolder) {
			return;
		}
		if (this.loadingPaths) {
			this.#mergePendingRefresh(changedSubset, topologyChanged);
			frontendLog(
				'fs-watcher',
				'debug',
				`refresh deferred (in-flight walk); subset=${changedSubset === null ? 'null' : changedSubset.size}, topology=${topologyChanged}`,
			);
			return;
		}
		try {
			if (topologyChanged) {
				await this.loadPaths(changedSubset);
			} else {
				// Modify-only batch: every changed entry already
				// exists in the tree's `paths` snapshot, so the
				// recursive `collect_paths` walk is wasted work.
				// Refresh git status (cheap aggregate IPC) and run
				// the per-buffer loop against the existing path
				// list. The narrowed loop visits only open files in
				// `changedSubset`, so the user's own Ctrl+S becomes
				// one `git status` call and one re-read instead of a
				// tree walk plus N×(git show + read).
				await this.refreshGitStatus(this.paths, changedSubset);
			}
		} finally {
			await this.#drainPendingRefresh();
		}
	}

	#mergePendingRefresh(changedSubset: ReadonlySet<string> | null, topologyChanged: boolean): void {
		this.#pendingRefreshTopology = this.#pendingRefreshTopology || topologyChanged;
		if (changedSubset === null) {
			// "Don't know what moved" wins: a `null` subset means
			// the next refresh has to sweep every open buffer, so
			// hold onto that hint and discard the narrower one.
			this.#pendingRefreshNullSubset = true;
			this.#pendingRefreshSubset = null;
			return;
		}
		if (this.#pendingRefreshNullSubset) {
			return;
		}
		if (this.#pendingRefreshSubset === null) {
			this.#pendingRefreshSubset = new Set(changedSubset);
			return;
		}
		for (const p of changedSubset) {
			this.#pendingRefreshSubset.add(p);
		}
	}

	async #drainPendingRefresh(): Promise<void> {
		if (!this.#pendingRefreshNullSubset && this.#pendingRefreshSubset === null) {
			return;
		}
		const subset = this.#pendingRefreshNullSubset ? null : this.#pendingRefreshSubset;
		const topology = this.#pendingRefreshTopology;
		this.#pendingRefreshSubset = null;
		this.#pendingRefreshNullSubset = false;
		this.#pendingRefreshTopology = false;
		await this.refreshActiveFolder(subset, topology);
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
				// Sample up to 5 paths so a `git checkout` burst
				// doesn't dump dozens of lines per emit; the user
				// can compare these against `file.path` in the
				// per-buffer log lines below to diagnose path
				// mismatches (workspace-relative vs absolute,
				// cross-folder, etc.).
				const sample = payload.paths.slice(0, 5).join(', ');
				const more = payload.paths.length > 5 ? ` … (+${payload.paths.length - 5} more)` : '';
				frontendLog(
					'fs-watcher',
					'info',
					`fs:changed paths=${payload.paths.length} topology=${payload.topologyChanged}${payload.paths.length === 0 ? '' : ` [${sample}${more}]`}`,
				);
				void this.refreshActiveFolder(subset, payload.topologyChanged);
				// `.git/MERGE_HEAD` / `.git/MERGE_MSG` appear when
				// the user starts a merge from a terminal (or
				// our own merge IPC) and disappear when it
				// commits / aborts. Either case flips the SCM
				// panel into / out of merge-in-progress mode,
				// so a dedicated re-probe keeps the panel live
				// without waiting for the next branch refresh
				// cadence. `refreshActiveFolder` above does
				// `refreshGitBranch → refreshGitMergeState` on
				// its own pass too; this is the surgical fast
				// path for the `.git/`-only burst.
				if (payload.paths.some((p) => p === '.git/MERGE_HEAD' || p === '.git/MERGE_MSG')) {
					void this.refreshGitMergeState();
				}
				// Forward the fs-watcher batch to every running
				// LSP server as `workspace/didChangeWatchedFiles`.
				// Each server filters the paths through the globs
				// it registered at startup, so a `.toml`-only
				// burst lands on no server at all and a TS file
				// change only reaches the TypeScript server.
				// Well-behaved servers respond by invalidating
				// caches and asking us (via the
				// `workspace/diagnostic/refresh` request handled
				// by the broker's notification pump) to re-pull
				// diagnostics for every open buffer — that's how
				// the panel catches up to a `git checkout`
				// without the user having to retype. Servers
				// that don't register watchers (rust-analyzer
				// post-init, push-only servers) silently no-op
				// inside the per-server filter, so the broad
				// fan-out is cheap. Git metadata paths stay out:
				// the watcher forwards `.git/HEAD` / `.git/refs/`
				// writes for the SCM panel, but no language
				// server wants per-commit `.git` churn — a
				// `**/*`-glob server would re-index for nothing.
				const lspPaths = payload.paths.filter((p) => p !== '.git' && !p.startsWith('.git/'));
				if (lspPaths.length > 0) {
					void ipc.lsp.notifyFilesChanged(lspPaths).catch((err) => {
						frontendLog('lsp.refresh', 'warn', `notifyFilesChanged failed: ${formatError(err)}`);
					});
				}
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
				// Cold-start safety net: an alt-tab back from a
				// terminal where the user just `git checkout`ed
				// before moon-ide was even running should clear
				// out stale LSP diagnostics. The fs-watcher
				// can't help here — it only fires for events
				// during its lifetime — so we re-pull every
				// open buffer on every running server. Cheap;
				// the broker debounces and push-only servers
				// noop the pull.
				this.scheduleLspDiagnosticsRefresh();
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
				// Update the per-producer slice for this path,
				// then recompute the flat union the editor reads.
				// Empty `diagnostics` from a producer is a real
				// clean-slate signal for that producer (server
				// finished a pass and the file has no problems
				// from its perspective) — we keep the producer
				// entry so the union explicitly reflects "ts ok,
				// oxlint still thinking" rather than reverting to
				// "ts hasn't reported yet".
				const nextByProducer = new Map(this.diagnosticsByProducer);
				const perPath = new Map(nextByProducer.get(payload.path) ?? new Map<string, LspDiagnostic[]>());
				perPath.set(payload.producer, payload.diagnostics);
				nextByProducer.set(payload.path, perPath);
				this.diagnosticsByProducer = nextByProducer;

				const flat: LspDiagnostic[] = [];
				for (const list of perPath.values()) {
					for (const d of list) {
						flat.push(d);
					}
				}
				const nextDiagnostics = new Map(this.diagnostics);
				nextDiagnostics.set(payload.path, flat);
				this.diagnostics = nextDiagnostics;
			});
			await listen<LspStatusEvent>('lsp:status', ({ payload }) => {
				const prev = this.lspStatuses.get(payload.languageId)?.status ?? null;
				const next = new Map(this.lspStatuses);
				next.set(payload.languageId, payload);
				this.lspStatuses = next;
				// If we just transitioned into `crashed` and the
				// active file is governed by this language id,
				// re-`open` it. The broker evicts the dead slot
				// on the next request; sending a fresh `open` now
				// primes the re-spawned server with the buffer's
				// current text so the user's *next* Ctrl+Space
				// resolves against a server that knows the doc.
				// Without this they'd have to switch tabs and
				// back, or edit a character, to re-attach.
				if (payload.status === 'crashed' && prev !== 'crashed') {
					this.#reopenActiveForLanguage(payload.languageId);
				}
			});
		} catch {
			// Event bus unavailable (tests / non-Tauri). The
			// editor will show no diagnostics and the status
			// pill will stay hidden — acceptable degradation.
		}
	}

	/**
	 * Schedule a workspace-wide LSP diagnostic re-pull on every
	 * running server, debounced 250ms. Used by the window-focus
	 * path to cover the cold-start case the in-IDE fs-watcher
	 * structurally can't — a `git checkout` that happened while
	 * moon-ide was closed leaves no fs event behind.
	 *
	 * 250ms: long enough that a rapid alt-tab pair collapses into
	 * one refresh, short enough that the user doesn't visibly wait
	 * between coming back to the window and the panel updating.
	 *
	 * In-IDE off-disk changes flow through
	 * `ipc.lsp.notifyFilesChanged` (in `bindFolderChangeRefresh`'s
	 * `fs:changed` branch) instead. The server decides when to ask
	 * for a refresh based on the watched-files notification, so
	 * the client-side scheduler isn't needed there.
	 */
	scheduleLspDiagnosticsRefresh(): void {
		if (this.#lspRefreshTimer !== null) {
			return;
		}
		this.#lspRefreshTimer = setTimeout(() => {
			this.#lspRefreshTimer = null;
			void ipc.lsp.refreshOpenDiagnostics([]).catch((err) => {
				// Best-effort — the user gets stale diagnostics
				// for one debounce window if the broker is mid-
				// rebuild, no worse than before this nudge
				// existed. Log so a real plumbing miswire shows
				// up in the diag-logs panel rather than being
				// silently swallowed.
				frontendLog('lsp.refresh', 'warn', `refreshOpenDiagnostics failed: ${formatError(err)}`);
			});
		}, 250);
	}

	#coderRefreshWired = false;
	#coderRefreshTimer: ReturnType<typeof setTimeout> | null = null;
	/**
	 * Folder paths that the parent agent's `tool_call` events have
	 * touched within the current debounce window. Drained on flush.
	 * Populated only when we can confidently resolve the target
	 * folder from `args.path`; ambiguous calls fall through to
	 * the all-folders fan-out by setting `#coderRefreshFanOut`.
	 */
	#coderRefreshPending: Set<string> = new Set();
	/**
	 * Sticky bit. Set when a sub-agent fires a `tool_call` (we
	 * don't have its bound folder in the event wrapper, so we
	 * can't be surgical), or when a parent `tool_call` couldn't
	 * be classified. Cleared on flush. When set, the next flush
	 * refreshes every bound folder.
	 */
	#coderRefreshFanOut = false;

	/**
	 * Subscribe to the coder event channel for project-bar refresh.
	 * The fs-watcher only sees the **active** folder, so an agent
	 * running in folder A that edits folder B (cross-folder path
	 * via `/workspace/<other>/...`, sub-agent on B, `bash` writing
	 * into B's tree, …) wouldn't otherwise trigger a status
	 * refresh — B's badge would lag until the user activated it.
	 *
	 * Surgical refresh strategy: every parent `tool_call` for a
	 * file-touching tool has its `args.path` parsed and resolved
	 * to a bound folder via the same `/workspace/<name>` rule
	 * the backend uses; that folder's path joins
	 * `#coderRefreshPending`. On `tool_result`, `turn_complete`,
	 * or `subagent_finished` we schedule a 200 ms debounced flush.
	 * Anything we can't confidently scope (sub-agent activity,
	 * `bash` writes, parse failures) flips a fan-out bit so the
	 * flush refreshes every bound folder — correctness over
	 * cleverness.
	 */
	async bindCoderRefresh(): Promise<void> {
		if (this.#coderRefreshWired) {
			return;
		}
		this.#coderRefreshWired = true;
		const flush = () => {
			this.#coderRefreshTimer = null;
			const pending = this.#coderRefreshPending;
			const fanOut = this.#coderRefreshFanOut;
			this.#coderRefreshPending = new Set();
			this.#coderRefreshFanOut = false;
			if (fanOut || pending.size === 0) {
				void this.refreshAllGitChangeSummaries();
				return;
			}
			for (const path of pending) {
				void this.refreshGitChangeSummary(path);
			}
		};
		const schedule = () => {
			if (this.#coderRefreshTimer !== null) {
				clearTimeout(this.#coderRefreshTimer);
			}
			this.#coderRefreshTimer = setTimeout(flush, 200);
		};
		try {
			await listen<CoderRefreshEnvelope>('coder:event', ({ payload }) => {
				const kind = payload.event.kind;
				if (kind === 'tool_call') {
					const target = this.resolveCoderEventTargetFolder(payload);
					if (target) {
						this.#coderRefreshPending.add(target);
					} else {
						this.#coderRefreshFanOut = true;
					}
					return;
				}
				if (kind === 'subagent_event') {
					// Sub-agent events don't carry the sub-agent's bound
					// folder in the wrapper, so any tool activity from a
					// sub-agent flips the fan-out bit. Cheap insurance.
					const inner = payload.event.inner;
					if (inner && inner.kind === 'tool_call') {
						this.#coderRefreshFanOut = true;
					}
					return;
				}
				if (kind === 'tool_result' || kind === 'turn_complete' || kind === 'subagent_finished') {
					schedule();
				}
			});
		} catch {
			// Event bus unavailable (tests / non-Tauri). Active-
			// folder fs-watcher fan-out still covers the in-folder
			// case; only the cross-folder agent edit goes unseen
			// until the next focus / palette refresh.
		}
	}

	/**
	 * Mirrors the backend's `resolve_workspace_path` so we can tell
	 * which bound folder a `tool_call` actually touches. Returns
	 * `null` when the tool isn't path-shaped (`bash`, `grep`,
	 * unknown), or when the path argument is missing — those flip
	 * the fan-out bit. `read_file` / `list_dir` / `write_file` /
	 * `edit_file` with a path-shaped argument (`path`, or compat-only
	 * `file_path` / `file` on `read_file` / `edit_file`) always resolve to *some*
	 * bound folder, falling back to the originating session's
	 * folder for unrouted relative paths.
	 */
	private resolveCoderEventTargetFolder(payload: CoderRefreshEnvelope): string | null {
		const ev = payload.event;
		if (ev.kind !== 'tool_call') {
			return null;
		}
		const tool = ev.name;
		if (tool !== 'read_file' && tool !== 'list_dir' && tool !== 'write_file' && tool !== 'edit_file') {
			return null;
		}
		const args = ev.args;
		let raw: string | null = null;
		if (args && typeof args === 'object') {
			const o = args;
			if (tool === 'read_file' || tool === 'edit_file') {
				const p = o.path ?? o.file_path ?? o.file;
				raw = typeof p === 'string' ? p : null;
			} else {
				raw = typeof o.path === 'string' ? o.path : null;
			}
		}
		if (!raw) {
			return null;
		}
		const folders = this.workspace?.folders ?? [];
		const synthetic = /^\/workspace\/([^/]+)/.exec(raw);
		if (synthetic) {
			const match = folders.find((f) => f.name === synthetic[1]);
			return match ? match.path : null;
		}
		if (raw.startsWith('./')) {
			return payload.folder;
		}
		const firstSegment = raw.split('/', 1)[0] ?? raw;
		const activeName = folders.find((f) => f.path === payload.folder)?.name;
		if (firstSegment !== activeName) {
			const sibling = folders.find((f) => f.name === firstSegment);
			if (sibling) {
				return sibling.path;
			}
		}
		return payload.folder;
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
		if (!languageId || isSyntheticBufferPath(path)) {
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
	 * Re-`open` the active editor buffer if it's governed by the
	 * broker slot identified by `languageId` (the slot key —
	 * `"typescript"`, `"rust"`, `"oxlint"`, …). Called from the
	 * `lsp:status` listener when a server crashes, so the broker's
	 * auto-respawn (lazy on the next request) lands with the live
	 * buffer text instead of an empty doc set.
	 *
	 * Membership goes through [`lspSlotCoversFile`] rather than a
	 * strict `lspLanguageFor(path) === languageId` test so the
	 * linter co-tenant works correctly: a crashed `oxlint` slot
	 * needs to reopen `.ts` / `.tsx` / `.js` / `.jsx` files, none
	 * of which produce a file-language id of `"oxlint"`.
	 */
	#reopenActiveForLanguage(languageId: string): void {
		const file = this.activeFile;
		if (file === null) {
			return;
		}
		if (isSyntheticBufferPath(file.path)) {
			return;
		}
		const fileLang = lspLanguageFor(file.path);
		if (fileLang === null || !lspSlotCoversFile(languageId, fileLang)) {
			return;
		}
		this.lspOpen(file.path, file.text);
	}

	/**
	 * Debounced `textDocument/didChange`. 150ms matches typical type
	 * cadence without making the server feel sluggish; longer and
	 * diagnostics lag behind what you see on screen, shorter and we
	 * spam the server during bursts (paste, autocomplete accept).
	 */
	lspScheduleUpdate(path: string, text: string) {
		const languageId = lspLanguageFor(path);
		if (!languageId || isSyntheticBufferPath(path)) {
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
	 * Force-send a `textDocument/didChange` for `path` with `text`
	 * right now, cancelling any pending debounced update. Used by
	 * the save path: the format-on-save pipeline can rewrite the
	 * bytes between what the LSP server last saw (via
	 * `lspScheduleUpdate`) and what the editor now shows, so we
	 * have to re-sync the server before its diagnostics are
	 * meaningful against the new buffer. Skipping the debounce
	 * matters because the pending timer's closure captured the
	 * *pre-format* text — letting it fire after a save would
	 * overwrite the server's view back to stale bytes.
	 */
	lspNotifyAfterSave(path: string, text: string) {
		const languageId = lspLanguageFor(path);
		if (!languageId || isSyntheticBufferPath(path)) {
			return;
		}
		const existing = this.#lspUpdateTimers.get(path);
		if (existing !== undefined) {
			clearTimeout(existing);
			this.#lspUpdateTimers.delete(path);
		}
		void ipc.lsp.update(path, languageId, text).catch(() => {});
	}

	/**
	 * Manually restart the LSP server for `languageId`. The backend
	 * drops the broker's server slot (next `lsp_*` request lazily
	 * re-spawns) and emits a `stopped` status; here we follow up
	 * by re-issuing `didOpen` for every currently-open buffer that
	 * maps to the same language id, so the user gets fresh
	 * diagnostics without having to flip tabs. Failures are
	 * surfaced via `flash` — the UX is "click the button, see a
	 * pill flip back to running within a couple seconds".
	 *
	 * The diagnostic cache for affected paths is **kept** rather
	 * than cleared; the new server publishes a full replacement
	 * list on its first analysis pass, so overwriting them at
	 * that moment is correct. Clearing now would leave a brief
	 * empty-state flash that the user would read as "the restart
	 * lost my errors".
	 */
	async restartLsp(languageId: string): Promise<void> {
		try {
			await ipc.lsp.restart(languageId);
		} catch (err) {
			this.flash(`Could not restart ${languageId} LSP: ${formatError(err)}`);
			return;
		}
		// Re-prime the new server with every open buffer it
		// governs. The broker is rooted at the active folder, so
		// cross-folder buffers will silently be NotAvailable —
		// that's fine, they were also NotAvailable before the
		// restart for the same reason. Membership is the slot's
		// covered set ([`lspSlotCoversFile`]), not a file-language
		// equality: a `restartLsp("oxlint")` has to reopen `.ts` /
		// `.tsx` / `.js` / `.jsx` buffers, whose own language ids
		// are `"typescript"` etc. — strict equality would silently
		// reopen zero files and the linter would come up empty
		// until the user typed in each tab again.
		for (const file of this.openFiles) {
			if (file.kind !== 'text' || file.isDeleted) {
				continue;
			}
			if (isSyntheticBufferPath(file.path)) {
				continue;
			}
			const fileLang = lspLanguageFor(file.path);
			if (fileLang === null || !lspSlotCoversFile(languageId, fileLang)) {
				continue;
			}
			this.lspOpen(file.path, file.text);
		}
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
		if (this.diagnosticsByProducer.has(path)) {
			const nextByProducer = new Map(this.diagnosticsByProducer);
			nextByProducer.delete(path);
			this.diagnosticsByProducer = nextByProducer;
		}
		if (!languageId || isSyntheticBufferPath(path)) {
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
		if (isSyntheticBufferPath(path)) {
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
		if (isSyntheticBufferPath(path)) {
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
		if (isSyntheticBufferPath(path)) {
			return;
		}
		if (this.#headInFlight.has(path)) {
			return;
		}
		this.#headInFlight.add(path);
		// In `'default'` baseline mode we read the file's content
		// at the merge-base instead of HEAD. The merge-base SHA
		// is cached on `FolderState.defaultBranchMergeBase` and
		// refreshed on every `refreshGitStatus` pass, so the
		// gutter / diff view automatically catch up when HEAD or
		// the default branch's tip moves.
		const mergeBase = this.defaultBranchMergeBase;
		const fetch =
			this.compareBaseline === 'default' && mergeBase !== null
				? ipc.fs.gitRefContent(mergeBase, path)
				: ipc.fs.gitHeadContent(path);
		void fetch
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
	 * editor focus — that's what tab clicks, file-tree clicks, the
	 * quick-open palette, and session restore all want. `{ focus: false }`
	 * is kept on the API for callers that want to update tabs without
	 * stealing focus (session-restore-style flows), but no surface
	 * currently exercises it. `focusedSide` updates either way so
	 * subsequent operations target the same pane.
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
			pendingEdit: null,
		};
		this.openFiles = [...this.openFiles, file];
		const tabs = this.tabsFor(side);
		this.setTabsFor(side, [...tabs, path]);
		this.setActive(path, side);
	}

	/**
	 * Open (or focus, if already open) the "Review changes" pseudo-
	 * tab for the active folder. The tab renders a stack of read-
	 * only diff sections against the default-branch merge-base —
	 * the entry point is the SCM panel's `vs main` row when the
	 * compare baseline is `'default'` and there are changes.
	 *
	 * Synthetic `OpenFile` carries empty bytes; everything routes
	 * off `workspace.gitStatusEntries` and
	 * `workspace.defaultBranchMergeBase` inside `ReviewView`. The
	 * path uses the `review://` prefix so persistence, LSP, blame,
	 * and HEAD fetch all skip it (see `isSyntheticBufferPath`).
	 */
	openReviewTab(side: SplitSide = this.focusedSide) {
		const path = REVIEW_PATH;
		const existing = this.openFiles.find((f) => f.path === path);
		if (!existing) {
			const file: OpenFile = {
				path,
				name: 'Review changes',
				kind: 'text',
				isUntitled: false,
				text: '',
				previewUrl: '',
				loadedFingerprint: fingerprint(''),
				loadedMtimeMs: null,
				isDirty: false,
				isDeleted: false,
				isExternal: false,
				pendingEdit: null,
			};
			this.openFiles = [...this.openFiles, file];
		}
		const tabs = this.tabsFor(side);
		if (!tabs.includes(path)) {
			this.setTabsFor(side, [...tabs, path]);
		}
		this.setActive(path, side);
	}

	/**
	 * Toggle the Review changes pseudo-tab on the focused side.
	 *
	 * - Off → on: same as [`openReviewTab`].
	 * - On  → off: jump to the file the user is currently looking
	 *   at in the review stack (see [`reviewVisibleFile`]) — switch
	 *   to its existing tab if one is open, or open a new one — then
	 *   close the review tab. With no tracked visible file (empty
	 *   review, no scroll yet) we fall back to a plain close, which
	 *   leaves `closeFile`'s usual neighbour-pick to decide focus.
	 *
	 * The SCM panel's review button calls this on click; the same
	 * button also flips its visual "pressed" state off the boolean
	 * `isReviewPath(activePath)` reactively, so going to another
	 * tab the regular way (clicking it in the tab strip) untoggles
	 * the button without going through this method at all.
	 */
	async toggleReviewTab(side: SplitSide = this.focusedSide) {
		const activeOnSide = side === 'left' ? this.leftActive : this.rightActive;
		const reviewActive = activeOnSide !== null && isReviewPath(activeOnSide);
		if (!reviewActive) {
			this.openReviewTab(side);
			return;
		}
		const target = this.reviewVisibleFile;
		this.reviewVisibleFile = null;
		if (target !== null && !isReviewPath(target)) {
			const tabs = this.tabsFor(side);
			if (tabs.includes(target)) {
				this.setActive(target, side);
				await this.closeFile(REVIEW_PATH, side);
				return;
			}
			await this.openFile(target, side);
		}
		await this.closeFile(REVIEW_PATH, side);
	}

	/**
	 * Open (or focus, if already open) a per-commit diff pseudo-tab.
	 * Each commit gets its own tab keyed on `commit://<sha>`; the
	 * `CommitView` component reads the SHA from the path and fetches
	 * the file list + blobs itself. Same synthetic-`OpenFile` shape as
	 * `openReviewTab` so persistence, LSP, blame, and HEAD fetch all
	 * skip it via `isSyntheticBufferPath`.
	 */
	openCommitTab(sha: string, subject: string, side: SplitSide = this.focusedSide) {
		const path = commitPath(sha);
		const existing = this.openFiles.find((f) => f.path === path);
		if (!existing) {
			const file: OpenFile = {
				path,
				name: subject.length > 0 ? subject : sha.slice(0, 7),
				kind: 'text',
				isUntitled: false,
				text: '',
				previewUrl: '',
				loadedFingerprint: fingerprint(''),
				loadedMtimeMs: null,
				isDirty: false,
				isDeleted: false,
				isExternal: false,
				pendingEdit: null,
			};
			this.openFiles = [...this.openFiles, file];
		}
		const tabs = this.tabsFor(side);
		if (!tabs.includes(path)) {
			this.setTabsFor(side, [...tabs, path]);
		}
		this.setActive(path, side);
	}

	/**
	 * Lazily attach a backing `OpenFile` for a path that the user is
	 * editing from inside a review section, without touching tab
	 * strips or the active-side pointer. The review tab stays
	 * focused; the underlying file just gains an in-memory buffer
	 * so [`updateText`] has a target and the section's edits can
	 * flow through the normal dirty-flag / fingerprint machinery.
	 *
	 * No-op when a buffer already exists. The file is loaded via
	 * the same [`loadTextFile`] path `openFile` uses, so blame /
	 * HEAD / editorconfig seeds and LSP `didOpen` fire exactly
	 * once — the user later switching to the regular editor tab
	 * for this file (via the section header's path click) is just
	 * a focus change, not a second load.
	 *
	 * Returns `true` on success, `false` if the load failed
	 * (caller decides whether to surface a toast or just keep
	 * the section read-only).
	 */
	async ensureBackingBuffer(path: string): Promise<boolean> {
		if (this.openFiles.some((f) => f.path === path)) {
			return true;
		}
		try {
			const next = await this.loadTextFile(path);
			if (!next) {
				return false;
			}
			this.openFiles = [...this.openFiles, next];
			if (next.kind === 'text' && !next.isDeleted) {
				this.lspOpen(next.path, next.text);
			}
		} catch (err) {
			this.flash(`Failed to open ${path}: ${formatError(err)}`);
			return false;
		}
		void this.ensureEditorConfig(path);
		return true;
	}

	/**
	 * Save a file that's being edited from inside a review section.
	 * The review tab is the active one, so [`saveActive`] would
	 * save the synthetic `review://` buffer instead — pointless
	 * write of zero bytes. This routes the underlying file's
	 * bytes through the same `fs.writeFile` + post-format re-read
	 * + LSP / blame / editorconfig refresh dance, just keyed off
	 * `path` instead of `activeFile`.
	 *
	 * No-op when the path has no backing buffer (defensive — the
	 * section's `updateListener` ensures one exists on first
	 * edit) or when the buffer is clean (nothing to write). Both
	 * cases are fine to silently skip; the section header's
	 * dirty pip already communicates the state to the user.
	 */
	async saveReviewSection(path: string): Promise<void> {
		const file = this.openFiles.find((f) => f.path === path);
		if (!file || file.kind !== 'text' || file.isUntitled || file.isExternal || file.isDeleted) {
			return;
		}
		if (!file.isDirty) {
			return;
		}
		try {
			const result = await ipc.fs.writeFile(path, file.text);
			const fresh = await ipc.fs.readFile(path);
			const freshText = fresh.is_binary ? file.text : fresh.text;
			this.openFiles = this.openFiles.map((f) =>
				f.path === path
					? {
							...f,
							text: freshText,
							isDirty: false,
							loadedFingerprint: fingerprint(freshText),
							loadedMtimeMs: result.mtime_ms,
						}
					: f,
			);
			this.lspNotifyAfterSave(path, freshText);
			if (file.name === '.editorconfig') {
				await this.refreshEditorConfigs();
			}
			this.scheduleBlameRefresh(path);
		} catch (err) {
			this.flash(`Save failed: ${formatError(err)}`);
		}
	}

	async openFile(path: string, side: SplitSide = this.focusedSide, options: { focus?: boolean } = {}) {
		// Nav-history (Alt+Left / forward) replays through `openFile`,
		// and the review pseudo-tab can sit in history just like a
		// real path. If the tab was closed in the meantime, the
		// synthetic OpenFile is gone from `openFiles` and the
		// fileKindFor / loadTextFile fall-through below would try to
		// IPC-load `review://default-branch` and fail. Re-route
		// review paths through `openReviewTab`, which rebuilds the
		// stub buffer on demand.
		if (isReviewPath(path)) {
			this.openReviewTab(side);
			return;
		}
		if (isCommitPath(path)) {
			// Commit pseudo-tab: the synthetic OpenFile is rebuilt
			// on demand if it was closed (same pattern as review).
			// We don't have the subject here, so fall back to the
			// SHA for the tab name; the tab title updates via the
			// existing file's `name` if the buffer already exists.
			const sha = shaFromCommitPath(path);
			if (sha !== null) {
				this.openCommitTab(sha, '', side);
			}
			return;
		}
		const existing = this.openFiles.find((f) => f.path === path);
		if (!existing) {
			const kind = fileKindFor(path);
			try {
				const next =
					kind === 'image' || kind === 'pdf' ? await this.loadPreviewFile(path, kind) : await this.loadTextFile(path);
				if (!next) {
					return;
				}
				// Re-check after the await before appending. The
				// `!existing` test above ran *before* `loadTextFile`
				// suspended, so two concurrent `openFile(path)` calls
				// (the file-tree click fires both `onSelectionChange`
				// *and* the wrapper-click handler — see
				// `activateRowFromTree`) could each pass that test and
				// then each append, corrupting `openFiles` with a
				// duplicate entry (observed in the wild). Only the call
				// that still doesn't see the buffer appends + opens the
				// LSP doc; the loser falls through to the shared
				// tab-add + activate below, which are idempotent.
				if (!this.openFiles.some((f) => f.path === path)) {
					this.openFiles = [...this.openFiles, next];
					// Notify the LSP broker only on first open —
					// reopening an already-loaded buffer is a pure UI
					// navigation event, the server still holds its open
					// state. Skip for deleted buffers: there's no
					// on-disk document for the server to track. Blame
					// fetch is handled by `setActive` (called below) —
					// that path covers session-restored files and
					// cross-folder jumps too.
					if (next.kind === 'text' && !next.isDeleted) {
						this.lspOpen(next.path, next.text);
					}
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
				pendingEdit: null,
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

	/** Flip soft-wrap for every editor pane. Each `Editor.svelte` has
	 *  a `$effect` that reads `workspace.lineWrap` and reconfigures
	 *  its line-wrap compartment, so a single state toggle propagates
	 *  to every visible buffer (and to any buffer mounted later). */
	toggleLineWrap() {
		this.lineWrap = !this.lineWrap;
		this.flash(this.lineWrap ? 'Line wrap on' : 'Line wrap off');
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
		if (isSyntheticBufferPath(path)) {
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
			pendingEdit: null,
		};
	}

	// Builds a read-only preview buffer for a binary file the webview can
	// render directly from disk via the Tauri asset protocol (images, PDFs).
	// The bytes never flow through `text`; the view component reads
	// `previewUrl`.
	private async loadPreviewFile(path: string, kind: 'image' | 'pdf'): Promise<OpenFile> {
		const absolute = await ipc.fs.absolutePath(path);
		return {
			path,
			name: basename(path),
			kind,
			isUntitled: false,
			text: '',
			previewUrl: convertFileSrc(absolute),
			loadedFingerprint: fingerprint(''),
			loadedMtimeMs: null,
			isDirty: false,
			isDeleted: false,
			isExternal: false,
			pendingEdit: null,
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
		// Same baseline-aware fetch as `refreshHead`: in
		// `'default'` mode the "before" side is the merge-base's
		// blob; otherwise it's HEAD's. A path that's gone from
		// disk + missing at the active baseline collapses to
		// `null` and the caller falls back to its disk-read
		// error message.
		const mergeBase = this.defaultBranchMergeBase;
		const headText =
			this.compareBaseline === 'default' && mergeBase !== null
				? await ipc.fs.gitRefContent(mergeBase, path)
				: await ipc.fs.gitHeadContent(path);
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
			pendingEdit: null,
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
		let next: OpenFile | null;
		try {
			next = await this.loadTextFile(path);
		} catch (err) {
			// `loadTextFile` throws when both the working-tree read and
			// the HEAD fallback fail (file vanished + not tracked, IPC
			// blew up mid-watch, …). Every caller does
			// `void reloadOpenFileFromDisk(…)` without a `.catch`, so
			// without this swallow each failed reload surfaces as an
			// unhandled promise rejection. Best-effort: keep the stale
			// buffer, log, move on.
			frontendLog('fs-watcher', 'warn', `reload of ${path} failed: ${String(err)}`);
			return;
		}
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
		// Closing a tab parked on a forwarded `$GIT_EDITOR` request
		// finishes the edit by default — same contract as closing
		// a saved file in any other editor: "I'm done." Cancel
		// requires an explicit affordance (right-click → Cancel
		// edit). `finishPendingEdit` clears `pendingEdit` and
		// re-invokes `closeFile`, which falls through to the
		// regular close path below. See ADR 0021.
		if (file.pendingEdit !== null) {
			await this.finishPendingEdit(path);
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
			// Closing the review tab for good drops its scroll-restore
			// snapshot so the next open starts at the top rather than
			// at a position from a now-stale session. Close runs in the
			// active folder, so its `FolderState` is the right target.
			if (isReviewPath(path)) {
				this.setReviewRestoreFor(this.activeFolderPath, null);
			}
			// External buffers were never opened with the LSP / git
			// machinery, so capture the flag before the filter and
			// skip the matching teardown. Calling `lspClose` here
			// would issue a `didClose` for a `didOpen` the broker
			// never saw.
			const closing = this.openFiles.find((f) => f.path === path);
			const wasExternal = closing?.isExternal === true;
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
			// Drop the per-buffer view-state snapshot. A reopen of
			// the same path should land at the start of the file
			// (matches every other IDE) rather than at wherever the
			// caret was the last time the buffer existed.
			const folder = this.activeFolderPath;
			if (folder !== null) {
				this.dropViewState(folder, path);
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
		// `focus` defaults to true so every navigation gesture (tab
		// click, tree-row click, palette quick-open, session restore)
		// lands in the editor ready to type. Callers can pass
		// `{ focus: false }` for the rare flow that wants to update
		// the tab strip without stealing focus.
		if (options.focus !== false) {
			this.requestEditorFocus();
		}
		this.persistAppState();
		this.scheduleEditorRenderCheck(side);
	}

	/**
	 * Watchdog for the intermittent "editor body frozen on the
	 * previous file" bug. `setActive` is plain imperative code, so it
	 * keeps running even when a pane's Svelte render effect has
	 * silently detached — which makes it the perfect place to verify,
	 * a beat later, that the DOM actually caught up with the state.
	 * `EditorPane` stamps what its template last committed onto
	 * `.body[data-view-path]`; if that disagrees with the active path
	 * once the flush + a couple frames have passed, we log a
	 * `runtime` error with a timestamp so the event is captured even
	 * if the user doesn't notice immediately. The 250ms delay is
	 * generous — a healthy flush commits within one frame — and the
	 * re-read of the *current* active path at check time means rapid
	 * tab-hopping can't produce false positives.
	 */
	private scheduleEditorRenderCheck(side: SplitSide) {
		setTimeout(() => {
			const expected = side === 'left' ? this.leftActive : this.rightActive;
			if (expected === null) {
				return;
			}
			const body = document.querySelector<HTMLElement>(`[data-region="editor-${side}"] .body`);
			if (body === null) {
				return;
			}
			const rendered = body.dataset.viewPath ?? '';
			if (rendered === expected) {
				return;
			}
			// Recovery nudge: bump the view tick so the pane's `view`
			// derived (which reads it) is force-invalidated and the
			// body recomputes from current state. With the tick now
			// wired into every editor-state setter this freeze should
			// no longer be reachable, but the nudge is cheap insurance
			// and re-checks below whether it actually recovered.
			this.editorViewTick++;
			// Capture the full diagnostic *now*, while the freeze is
			// live — the user may switch folders (which un-freezes the
			// pane and destroys the evidence) before thinking to run
			// the palette command. The toast makes the detection
			// visible; the dump in the `runtime` log makes it
			// actionable later. Re-read after a frame so the dump
			// reflects whether the nudge above un-stuck it.
			setTimeout(() => {
				const stillStale = (body.dataset.viewPath ?? '') !== (side === 'left' ? this.leftActive : this.rightActive);
				frontendLog(
					'runtime',
					stillStale ? 'error' : 'warn',
					`editor render watchdog: pane=${side} detected stale render (DOM=${rendered || '∅'}, ` +
						`state=${expected}); after tick nudge ${stillStale ? 'STILL STALE' : 'recovered'}.\n${this.dumpEditorState()}`,
				);
				if (stillStale) {
					this.flash('Editor render bug detected — diagnostic captured (runtime logs)');
				}
			}, 0);
		}, 250);
	}

	/**
	 * Diagnostic snapshot for the frozen-editor-pane bug. Compares,
	 * per pane: the raw workspace state (source of truth), what
	 * `EditorPane`'s template last committed to the DOM
	 * (`data-view-path` / `data-view-kind`), and which buffer the
	 * CodeMirror state actually holds (`data-cm-path`, stamped
	 * imperatively by `Editor.svelte`'s swap effect). Whichever pair
	 * diverges names the frozen layer: `STALE-TEMPLATE` means the
	 * pane's render effect detached; `STALE-CM-SWAP` means the
	 * template committed but the editor's in-place swap didn't run.
	 * Called by the watchdog above (automatically, on detection) and
	 * by the "Debug: Dump Editor State" palette command (manually).
	 */
	dumpEditorState(): string {
		const lines: string[] = [`editor-state dump @ ${new Date().toISOString()}`];
		lines.push(`activeFolder=${this.activeFolderPath ?? '∅'}`);
		lines.push(`focusedSide=${this.focusedSide} hasSplit=${this.hasSplit}`);
		lines.push(`openFiles=[${this.openFiles.map((f) => `${f.path}${f.isDirty ? '*' : ''}`).join(', ')}]`);
		for (const side of ['left', 'right'] as const) {
			if (side === 'right' && !this.hasSplit) {
				continue;
			}
			const active = side === 'left' ? this.leftActive : this.rightActive;
			const tabs = this.tabsFor(side);
			const body = document.querySelector<HTMLElement>(`[data-region="editor-${side}"] .body`);
			const cmHost = body?.querySelector<HTMLElement>('[data-cm-path]') ?? null;
			const viewPath = body?.dataset.viewPath ?? '(no body el)';
			const viewKind = body?.dataset.viewKind ?? '(no body el)';
			const cmPath = cmHost?.dataset.cmPath ?? '(no editor el)';
			const stateVsDom = active === (viewPath === '' ? null : viewPath) ? 'OK' : 'STALE-TEMPLATE';
			const domVsCm = cmHost === null || viewPath === cmPath ? 'OK' : 'STALE-CM-SWAP';
			lines.push(
				`pane=${side} state.active=${active ?? '∅'} tabs=[${tabs.join(', ')}]\n` +
					`  dom.viewPath=${viewPath} dom.viewKind=${viewKind} dom.cmPath=${cmPath}\n` +
					`  template=${stateVsDom} cmSwap=${domVsCm}`,
			);
		}
		return lines.join('\n');
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

	/** True iff some open pane currently shows the Review pseudo-tab.
	 *  Drives the SCM filter tree's click semantics — when this is
	 *  `true`, a row click scrolls the review view rather than
	 *  opening a new editor tab. */
	get isReviewTabVisible(): boolean {
		return this.leftActive === REVIEW_PATH || (this.hasSplit && this.rightActive === REVIEW_PATH);
	}

	/** Ask the open `ReviewView` to scroll its `path` section into
	 *  view. Bumps `tick` even on repeat-same-path clicks so the
	 *  reactivity round-trips. No-op if no review tab is currently
	 *  open — the caller decides whether to also open one. */
	requestReviewScroll(path: string) {
		const tick = (this.reviewScrollRequest?.tick ?? 0) + 1;
		this.reviewScrollRequest = { path, tick };
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
		// Review tab is the active tab when the user pressed Ctrl+S
		// from inside a review section's editor. The synthetic
		// `review://` buffer has nothing useful to save (it's
		// always-clean empty bytes); route the keystroke to the
		// underlying file the focused section is editing. Falls
		// through to the regular save-active path when no section
		// has focus, which then no-ops on the clean synthetic
		// buffer.
		if (isReviewPath(file.path) && this.reviewFocusPath !== null) {
			await this.saveReviewSection(this.reviewFocusPath);
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
		// Buffers parked on a forwarded `$GIT_EDITOR` request finish
		// the edit instead of doing a regular save: write the bytes
		// host-direct, reply `OK\n` on the parked socket, close the
		// tab so the in-container `git commit` continues. See ADR 0021.
		if (file.pendingEdit) {
			await this.finishPendingEdit(file.path);
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
			// Capture pre-save conflict status so the auto-stage
			// branch below knows whether to look at the post-save
			// markers. Reading after the IPC could race a watcher
			// burst that flips the status while we're awaiting.
			const wasConflicted = this.gitStatusEntries.some(
				(entry) => entry.path === file.path && entry.status === 'conflicted',
			);
			const result = await ipc.fs.writeFile(file.path, file.text);
			// The pre-save pipeline (line endings, trim trailing ws, final
			// newline) runs server-side, so the bytes on disk may not equal
			// `file.text`. Re-read so the in-memory buffer matches disk and
			// the dirty fingerprint reflects the canonical form. Otherwise
			// "save then save again" would still mark dirty if the pipeline
			// changed anything.
			const fresh = await ipc.fs.readFile(file.path);
			const freshText = fresh.is_binary ? file.text : fresh.text;
			// Auto-stage during merge resolution: if the file was
			// reported `conflicted` on entry and the saved bytes
			// no longer carry any column-0 conflict markers, run
			// `git add` so the unmerged index entry clears. Without
			// this the row's conflict badge would persist (and
			// commit-merge would refuse) until either the user
			// clicked commit (which `git add -A`s the whole tree)
			// or ran `git add` from a terminal. Best-effort: a
			// failed `git add` is silent — the next status refresh
			// will surface any persistent index mismatch.
			if (wasConflicted && !hasConflictMarkerLines(freshText)) {
				try {
					await ipc.fs.gitAddPaths([file.path]);
				} catch {
					// Silent: the badge stays, the next refresh
					// re-evaluates. We don't want to flash a toast
					// for a transient `git add` failure during
					// what feels like a routine save.
				}
				void this.refreshGitMergeState();
			}
			this.openFiles = this.openFiles.map((f) =>
				f.path === file.path
					? {
							...f,
							text: freshText,
							isDirty: false,
							loadedFingerprint: fingerprint(freshText),
							loadedMtimeMs: result.mtime_ms,
						}
					: f,
			);
			// Re-sync the LSP server to the post-format bytes. The
			// debounced `lspScheduleUpdate` from the last keystroke
			// (if any) carries the *pre-format* text in its closure,
			// so without this the server would either stay on the
			// stale text or — worse — have its view dragged back to
			// the pre-format bytes when the pending timer fires
			// *after* the save. Either case leaves the squigglies
			// stuck on positions that no longer match what the
			// editor displays. Binary files don't get an
			// `lspLanguageFor` mapping so this is a no-op for them.
			this.lspNotifyAfterSave(file.path, freshText);
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
		if (file.kind === 'image' || file.kind === 'pdf') {
			// Image / PDF buffers are read-only previews; "Save As" would
			// need us to copy bytes through the host, which nobody has
			// asked for yet. Refuse loudly rather than half-implement.
			this.flash(`Save As is not supported for ${file.kind} files.`);
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
 * How deep `fs_collect_paths` recurses. The walk is `read_dir`-
 * bound and gitignore-collapsed (`node_modules/` etc. only emit
 * a single row), so 16 levels comfortably covers every realistic
 * project — SvelteKit's deepest `[param]/` route stacks fit, and
 * monorepos with `packages/<scope>/<name>/src/**` don't get cut
 * short. Anything that *does* hit the cap surfaces in
 * `CollectPathsResult.depth_capped`; FileTree marks those rows as
 * lazy and fetches their children on expansion via
 * `collect_paths_under`. Bumping the cap rather than removing it
 * is defence-in-depth against pathologically deep gitignore-
 * leaking trees we haven't seen in the wild yet.
 */
const MAX_TREE_DEPTH = 16;

/**
 * Quick check for column-0 merge-conflict marker lines —
 * `<<<<<<<`, `=======`, `>>>>>>>`. Used by `saveActive` to
 * decide whether a file that was conflicted has actually had
 * its markers cleared before running `git add` against it.
 *
 * Scans every line because a buffer may legitimately have only
 * the closing marker present if the user deleted the opening
 * one without the rest — git would still report the file as
 * unmerged, and auto-staging it would leave a half-resolved
 * commit. Conservative: any marker line at column 0 vetoes
 * the auto-stage, the user resolves the rest from the row.
 */
function hasConflictMarkerLines(text: string): boolean {
	let cursor = 0;
	while (cursor < text.length) {
		// `\n` only — git rewrites CRLF on porcelain output and
		// our format-on-save normalises line endings before this
		// runs.
		const nl = text.indexOf('\n', cursor);
		const end = nl === -1 ? text.length : nl;
		// `startsWith` on a substring would allocate; check by
		// index instead. `<` / `=` / `>` all need at least seven
		// consecutive copies to match the marker.
		if (matchesMarker(text, cursor, '<') || matchesMarker(text, cursor, '=') || matchesMarker(text, cursor, '>')) {
			return true;
		}
		if (nl === -1) {
			return false;
		}
		cursor = end + 1;
	}
	return false;
}

function matchesMarker(text: string, lineStart: number, ch: string): boolean {
	for (let i = 0; i < 7; i++) {
		if (text.charAt(lineStart + i) !== ch) {
			return false;
		}
	}
	return true;
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

/** True when the fs-watcher's changed-path set includes one of the
 *  `.git/` top-level files it forwards for history-pointer moves.
 *  Those writes shift what `git blame` attributes to each line even
 *  when no working-tree file changes, so `refreshGitStatus` uses
 *  this to decide whether to re-blame open buffers that weren't
 *  themselves touched on disk. */
function subsetTouchesGitState(subset: ReadonlySet<string>): boolean {
	for (const p of subset) {
		if (isGitStatePath(p)) {
			return true;
		}
	}
	return false;
}

/** Matches the backend allowlist in `fs_watcher.rs`
 *  (`is_dotgit_observed_top_level`): a `.git/`-relative `HEAD`
 *  (branch switch / checkout), `MERGE_HEAD` / `MERGE_MSG` (a
 *  merge starting / finishing), or `index` (commit / restore /
 *  reset — the index rewrites even when no working-tree file
 *  changes). Path is forward-slash, workspace-relative. */
function isGitStatePath(path: string): boolean {
	const parts = path.split('/');
	const last = parts[parts.length - 1];
	if (last !== 'HEAD' && last !== 'MERGE_HEAD' && last !== 'MERGE_MSG' && last !== 'index') {
		return false;
	}
	return parts[parts.length - 2] === '.git';
}

/** True for any synthetic path that doesn't back onto a real
 *  on-disk file under a bound folder — `untitled:N` unsaved
 *  buffers, the `review://` pseudo-tab, and `commit://<sha>` per-
 *  commit diff tabs. Used by every gate that would otherwise route
 *  to the host's filesystem (LSP, blame, HEAD fetch, persistence,
 *  format-on-save, …) so we don't fire IPCs that are guaranteed to
 *  fail or, worse, silently match the wrong file. */
export function isSyntheticBufferPath(path: string): boolean {
	return isUntitledPath(path) || isReviewPath(path) || isCommitPath(path);
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
 * Content fingerprint for a review comment's anchored line(s)
 * (Phase 5.7). The anchor's line numbers are a render hint; this
 * fingerprint is the truth used to re-locate the anchor after the
 * text drifts and, at publish time, to confirm the line still
 * exists at the PR head. We trim each line and join with `\n` so
 * pure indentation shifts (a reformat that re-indents the block)
 * don't lose the anchor, then hash with the FNV-1a 32-bit variant —
 * a tiny, dependency-free, deterministic hash; collision risk over
 * a handful of nearby lines is negligible for a positioning hint.
 */
function reviewFingerprint(lineText: string): string {
	const normalized = lineText
		.split('\n')
		.map((l) => l.trim())
		.join('\n');
	let hash = 0x811c9dc5;
	for (let i = 0; i < normalized.length; i++) {
		hash ^= normalized.charCodeAt(i);
		// FNV prime, kept in 32-bit range via Math.imul.
		hash = Math.imul(hash, 0x01000193);
	}
	return (hash >>> 0).toString(16).padStart(8, '0');
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
