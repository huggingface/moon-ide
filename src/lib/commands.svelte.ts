import { confirm, open } from '@tauri-apps/plugin-dialog';
import { workspace } from './state.svelte';
import { coder } from './coder.svelte';
import { slack } from './slack.svelte';
import { ipc } from './ipc';
import { formatError, type FileSearchResult, type ContentSearchHit } from './protocol';
import { isMarkdownPath } from './util/markdown';

export type Command = {
	id: string;
	/**
	 * Display label. Either a fixed string or a getter so commands can
	 * reflect live state (e.g. "Switch to Light Theme" flips after each
	 * toggle). The palette calls it once per render, so cheap reads only.
	 */
	title: string | (() => string);
	shortcut?: string;
	/**
	 * When set, the command is hidden from the palette unless it returns
	 * true. Use for commands that only make sense in a particular
	 * context (e.g. "Toggle Markdown Preview" needs an active markdown
	 * tab). Same cheap-read rule as `title` — called once per filter.
	 */
	visible?: () => boolean;
	run: () => void | Promise<void>;
};

export function commandTitle(cmd: Command): string {
	return typeof cmd.title === 'function' ? cmd.title() : cmd.title;
}

/**
 * Pre-fill string for the "Search in Files…" palette. Mirrors
 * VS Code / Cursor: opening the workspace search with a non-empty
 * editor selection drops the selected text into the search box so
 * the user can hit Enter immediately. Multi-line selections
 * collapse to their first non-empty line — a multi-line ripgrep
 * pattern is rarely what the user meant, and the palette input is
 * single-line anyway. Returns `''` when there's nothing usable
 * (no selection, blank-only selection, or a payload long enough
 * that pre-filling would dominate the input field).
 */
export function searchQueryFromSelection(): string {
	const sel = workspace.activeSelection;
	if (sel === null) {
		return '';
	}
	for (const line of sel.text.split(/\r?\n/)) {
		const trimmed = line.trim();
		if (trimmed.length === 0) {
			continue;
		}
		if (trimmed.length > 200) {
			return '';
		}
		return trimmed;
	}
	return '';
}

export type PaletteMode = 'commands' | 'files' | 'search';

class PaletteState {
	open = $state(false);
	mode = $state<PaletteMode>('commands');
	query = $state('');

	fileResults = $state<FileSearchResult[]>([]);
	contentResults = $state<ContentSearchHit[]>([]);
	contentTruncated = $state(false);
	loading = $state(false);

	// Per-session search toggles, mirroring VS Code's `Aa | \b | .*`
	// trio plus a path-scope input. They live on the palette state
	// (not on `WorkspaceState`) so the user's choices survive
	// closing-and-reopening the palette during one IDE session but
	// don't bleed into other windows. No persistence across IDE
	// launches yet — wait for someone to ask.
	searchCaseSensitive = $state(false);
	searchWholeWord = $state(false);
	searchRegex = $state(false);
	/** Gitignore-style glob restricting the walk. Empty string =
	 *  "no scope filter". Bare paths like `src/lib` are normalised
	 *  to `src/lib/**` server-side. */
	searchInclude = $state('');
	/** Mass-replace input. Empty string + `replaceOpen=false` keep
	 *  the palette in plain search mode; flipping `replaceOpen` shows
	 *  a second row + a "Replace All" button. We don't auto-show on
	 *  Ctrl+Shift+F so the common "find references" use case still
	 *  opens to the simplest layout — users opt in to refactor mode
	 *  with the toggle button (or `Ctrl+H`). */
	replaceOpen = $state(false);
	replaceText = $state('');
	/** Running a replace can block the UI for a couple of seconds on
	 *  a large repo. The flag pumps a spinner + disables the button
	 *  so users don't double-fire the same refactor. */
	replaceRunning = $state(false);

	show(mode: PaletteMode, initialQuery = '') {
		this.mode = mode;
		this.query = initialQuery;
		this.open = true;
		this.fileResults = [];
		this.contentResults = [];
		this.contentTruncated = false;
	}

	hide() {
		this.open = false;
	}

	setQuery(q: string) {
		this.query = q;
	}

	setSearchInclude(value: string) {
		this.searchInclude = value;
	}

	toggleSearchCaseSensitive() {
		this.searchCaseSensitive = !this.searchCaseSensitive;
	}

	toggleSearchWholeWord() {
		this.searchWholeWord = !this.searchWholeWord;
	}

	toggleSearchRegex() {
		this.searchRegex = !this.searchRegex;
	}

	setReplaceText(value: string) {
		this.replaceText = value;
	}

	setReplaceOpen(value: boolean) {
		this.replaceOpen = value;
	}

	toggleReplaceOpen() {
		this.replaceOpen = !this.replaceOpen;
	}
}

export const palette = new PaletteState();

export const builtInCommands: Command[] = [
	{
		id: 'workspace.openFolder',
		title: 'Add Folder…',
		run: async () => {
			const selected = await open({ directory: true, multiple: false });
			if (typeof selected === 'string') {
				await workspace.openLocal(selected);
			}
		},
	},
	{
		id: 'workspace.refreshTree',
		title: 'Refresh File Tree',
		run: () => void workspace.loadPaths(),
	},
	{
		id: 'editor.openFile',
		title: 'Open File…',
		shortcut: 'Ctrl+O',
		// Mirrors the `Ctrl+O` keybinding in App.svelte. The native
		// dialog runs on the host machine even when the active
		// folder lives in a container, and `openHostFile` routes
		// the picked path through `fs.readFileHost` when it falls
		// outside every bound folder — so the user can pop open
		// any host file (a sibling repo, ~/.bashrc, …) without
		// adding the folder to the workspace.
		run: async () => {
			if (!workspace.workspace) {
				workspace.flash('Open a folder before opening a file.');
				return;
			}
			const selected = await open({ directory: false, multiple: false });
			if (typeof selected === 'string') {
				await workspace.openHostFile(selected);
			}
		},
	},
	{
		id: 'editor.newFile',
		title: 'New File',
		shortcut: 'Ctrl+N',
		// Mirrors the Ctrl+N handler in App.svelte — we refuse to spawn
		// untitled tabs without a workspace because there's no editor
		// pane to host them. The keyboard handler shows a toast in that
		// case; doing nothing here is fine since the command is
		// reachable from the palette only after a folder is open
		// anyway, but the guard is cheap and keeps the two entry
		// points symmetric.
		run: () => {
			if (!workspace.workspace) {
				workspace.flash('Open a folder before creating a new file.');
				return;
			}
			workspace.newUntitledTab();
		},
	},
	{
		id: 'editor.save',
		title: 'Save File',
		shortcut: 'Ctrl+S',
		run: () => void workspace.saveActive(),
	},
	{
		id: 'editor.saveAs',
		// "Save As" promotes an untitled buffer or rebinds an existing
		// file to a new path. No keyboard shortcut yet: Ctrl+Shift+S is
		// the natural pick but we hold off until someone asks (scope
		// discipline). Discoverable from the palette in the meantime.
		title: 'Save File As…',
		run: () => void workspace.saveActiveAs(),
	},
	{
		id: 'palette.quickOpen',
		title: 'Go to File…',
		shortcut: 'Ctrl+P',
		run: () => palette.show('files'),
	},
	{
		id: 'palette.searchInFiles',
		title: 'Search in Files…',
		shortcut: 'Ctrl+Shift+F',
		run: () => {
			// Plain "find in files" — close the replace row if a
			// previous refactor session left it open, so the user
			// who pressed Ctrl+Shift+F lands in the simplest layout.
			palette.setReplaceOpen(false);
			palette.show('search', searchQueryFromSelection());
		},
	},
	{
		id: 'palette.replaceInFiles',
		title: 'Replace in Files…',
		// VS Code / IntelliJ both put mass-replace on Ctrl+Shift+H,
		// and the team is migrating from those tools — keep the
		// muscle memory. Identical to "Search in Files" but opens
		// with the replace row visible and the replace input
		// focused.
		shortcut: 'Ctrl+Shift+H',
		run: () => {
			palette.setReplaceOpen(true);
			palette.show('search', searchQueryFromSelection());
		},
	},
	{
		id: 'git.switchBranch',
		title: 'Switch Branch…',
		shortcut: 'Ctrl+Shift+B',
		visible: () => workspace.workspace !== null,
		run: () => workspace.openBranchSwitcher(),
	},
	{
		id: 'editor.toggleLineWrap',
		title: 'Toggle Line Wrap',
		shortcut: 'Alt+Z',
		run: () => workspace.toggleLineWrap(),
	},
	{
		id: 'editor.splitRight',
		title: 'Split Editor Right',
		// Same key handles both directions because `Ctrl+\` in App.svelte
		// is a toggle (split if not split, close if already split). Both
		// commands advertise it so users find it from either entry.
		shortcut: 'Ctrl+\\',
		run: () => workspace.splitActive('right'),
	},
	{
		id: 'editor.closeSplit',
		title: 'Close Secondary Split',
		shortcut: 'Ctrl+\\',
		run: () => workspace.closeSplit(),
	},
	// Nav-history entry points. Hidden when there's nowhere to go —
	// an always-visible "Go Back" that sometimes does nothing trains
	// users to ignore it. The shortcut label is what most users will
	// actually rely on; the palette entry is mostly for discovery.
	{
		id: 'nav.goBack',
		title: 'Go Back',
		shortcut: 'Alt+Left',
		visible: () => workspace.canNavigateBack,
		run: () => void workspace.navigateBack(),
	},
	{
		id: 'nav.goForward',
		title: 'Go Forward',
		shortcut: 'Alt+Right',
		visible: () => workspace.canNavigateForward,
		run: () => void workspace.navigateForward(),
	},
	// Three explicit entries rather than a cycling toggle: the
	// underlying setting is a three-way enum (System / Dark /
	// Light) and "cycle" has no obvious order. Keeping one item
	// per value also makes each mode fuzzy-searchable from the
	// palette. The currently-active one is suffixed with
	// "(current)" so filtering on e.g. "dark" surfaces whether
	// you're already there.
	{
		id: 'theme.system',
		title: () => (workspace.theme === 'system' ? 'Theme: System (current)' : 'Theme: System'),
		run: () => workspace.setTheme('system'),
	},
	{
		id: 'theme.dark',
		title: () => (workspace.theme === 'dark' ? 'Theme: Dark (current)' : 'Theme: Dark'),
		run: () => workspace.setTheme('dark'),
	},
	{
		id: 'theme.light',
		title: () => (workspace.theme === 'light' ? 'Theme: Light (current)' : 'Theme: Light'),
		run: () => workspace.setTheme('light'),
	},
	{
		id: 'editor.autocomplete',
		title: 'Editor: Autocomplete (Ctrl+T)',
		run: () => workspace.requestAutocomplete(),
	},
	{
		// `Focus File Tree` is the only discrete focus command — it
		// always means the same thing wherever you invoke it from.
		// The cycle commands (`F6` / `Shift+F6`) are intentionally
		// keyboard-only: the palette is off-region for the cycle, so
		// invoking them from there always re-enters at the same edge
		// rather than advancing relative to where the user was. Use
		// the keys directly.
		id: 'focus.sidebar',
		title: 'Focus File Tree',
		shortcut: 'Ctrl+0',
		run: () => workspace.requestSidebarFocus(),
	},
	{
		id: 'view.reloadWindow',
		// Refreshes the webview only — the Rust shell stays alive.
		// Persisted state (workspace + tabs + active + theme) replays
		// from AppState on the way back up, so visually the only
		// difference is in-memory edits to dirty buffers vanish (we
		// prompt for confirmation when there are any). Mirrors the
		// browser-level "reload" the team currently triggers from the
		// webview's right-click menu; lives here so the right-click
		// menu can be locked down later without losing the escape
		// hatch.
		title: 'Reload Window',
		shortcut: 'Ctrl+R',
		run: () => reloadWindow(),
	},
	{
		id: 'files.refreshTree',
		// Re-enumerate the active folder's files and re-classify git
		// status. The window-focus auto-refresh covers changes made
		// in an external terminal, but the integrated terminal
		// doesn't trigger focus events, so commands like `git
		// checkout HEAD -- foo` run inside the IDE need this
		// explicit nudge to update the tree's badges and ghost
		// rows. No shortcut by default — the refresh is
		// usually unnecessary.
		title: 'Refresh File Tree',
		run: () => workspace.refreshActiveFolder(),
	},
	{
		id: 'coder.togglePanel',
		// Ctrl+L mirrors Cursor's "open chat" gesture. With a
		// selection in the editor it attaches the range as a chip;
		// without one it just toggles visibility. The palette
		// entry doesn't carry the selection-attach behaviour
		// (palette dispatch isn't tied to editor focus), so it
		// only ever toggles.
		title: () => (coder.panelVisible ? 'Coder: Hide Panel' : 'Coder: Show Panel'),
		shortcut: 'Ctrl+L',
		run: () => coder.togglePanel(),
	},
	{
		id: 'chat.togglePanel',
		// Wording flips with panel state — same diagnostic value as
		// the theme toggle, and means the user knows which way the
		// command goes before clicking. No keyboard shortcut: Ctrl+L
		// went to the coder panel (mirroring Cursor); slack reaches
		// from the status-bar pip and the speech-bubble icon on the
		// coder header.
		title: () => (slack.panelVisible ? 'Chat: Hide Panel' : 'Chat: Show Panel'),
		run: () => slack.togglePanel(),
	},
	{
		id: 'chat.connect',
		title: 'Chat: Connect Slack…',
		// Only useful before the user has connected — once a token is
		// in the keyring, the modal is replaced by the Disconnect
		// affordance inside the panel itself.
		visible: () => !slack.connected,
		run: () => {
			slack.setPanelVisible(true);
			slack.openConnectModal();
		},
	},
	{
		id: 'chat.disconnect',
		title: 'Chat: Disconnect Slack',
		visible: () => slack.connected,
		run: () => void slack.disconnect(),
	},
	{
		id: 'markdown.togglePreview',
		// Label flips with the current mode for the active path so the
		// palette doubles as a status indicator. Only visible when the
		// active tab is a markdown file — for every other path, the
		// toggle button at the right end of the tab strip wouldn't be
		// shown either, so the command shouldn't be reachable.
		title: () => {
			const path = workspace.activePath;
			if (path !== null && workspace.previewModeFor(path) === 'preview') {
				return 'Markdown: Show Source';
			}
			return 'Markdown: Show Preview';
		},
		visible: () => {
			const path = workspace.activePath;
			return path !== null && isMarkdownPath(path);
		},
		run: () => {
			const path = workspace.activePath;
			if (path === null) {
				return;
			}
			workspace.togglePreviewMode(path);
		},
	},
	{
		id: 'git.toggleDiffView',
		// Title flips with the current mode so the palette entry is
		// self-describing.
		title: () => {
			const path = workspace.activePath;
			return path !== null && workspace.diffModeFor(path) ? 'Git: Hide Diff View' : 'Git: View Diff';
		},
		shortcut: 'Ctrl+Shift+D',
		// Visible when the active file is a **modified** working-
		// tree change (the only case where there's a meaningful HEAD
		// vs working tree diff to flip into). Untracked / added /
		// ignored files have no `HEAD` side. Deleted files now
		// render as a read-only `Editor` of the HEAD blob by
		// default — the explicit "View diff" right-click in the
		// file tree is the path to the side-by-side for the rare
		// "show me HEAD vs empty" case, so the palette command
		// stays hidden here to keep it focused on the common flow.
		visible: () => {
			const path = workspace.activePath;
			if (path === null) {
				return false;
			}
			const file = workspace.openFiles.find((f) => f.path === path);
			if (!file || file.kind !== 'text' || file.isDeleted || file.isUntitled) {
				return false;
			}
			const entry = workspace.gitStatusEntries.find((e) => e.path === path);
			return entry?.status === 'modified';
		},
		run: () => {
			const path = workspace.activePath;
			if (path === null) {
				return;
			}
			workspace.toggleDiffMode(path);
		},
	},
];

export function filterCommands(query: string): Command[] {
	const visible = builtInCommands.filter((c) => (c.visible ? c.visible() : true));
	const q = query.trim().toLowerCase();
	if (!q) {
		return visible;
	}
	return visible
		.map((c) => ({ c, score: scoreString(commandTitle(c).toLowerCase(), q) }))
		.filter((x) => x.score > 0)
		.toSorted((a, b) => b.score - a.score)
		.map((x) => x.c);
}

function scoreString(haystack: string, needle: string): number {
	if (haystack === needle) {
		return 1_000_000;
	}
	if (haystack.startsWith(needle)) {
		return 30_000;
	}
	if (haystack.includes(needle)) {
		return 10_000;
	}
	let score = 0;
	let i = 0;
	for (const c of needle) {
		const idx = haystack.indexOf(c, i);
		if (idx < 0) {
			return 0;
		}
		score += 100 - (idx - i);
		i = idx + 1;
	}
	return score;
}

/**
 * Confirm-and-reload. Exported so App.svelte's `Ctrl+Shift+R`
 * handler can share the dirty-buffer prompt with the palette
 * command — keep the two entry points calling the same function.
 */
export async function reloadWindow() {
	const dirty = workspace.openFiles.filter((f) => f.isDirty);
	if (dirty.length > 0) {
		const ok = await confirm(
			`${dirty.length} file${dirty.length === 1 ? ' has' : 's have'} unsaved changes. Reload and discard them?`,
			{ title: 'Unsaved changes', okLabel: 'Reload', cancelLabel: 'Cancel' },
		);
		if (!ok) {
			return;
		}
	}
	location.reload();
}

export async function runFileSearch(query: string) {
	if (!workspace.workspace) {
		return;
	}
	palette.loading = true;
	try {
		palette.fileResults = await ipc.search.files({ query, limit: 50 });
	} catch (err) {
		workspace.flash(`Search failed: ${formatError(err)}`);
		palette.fileResults = [];
	} finally {
		palette.loading = false;
	}
}

export async function runContentSearch(query: string) {
	if (!workspace.workspace) {
		return;
	}
	palette.loading = true;
	try {
		const include = palette.searchInclude.trim();
		const result = await ipc.search.content({
			query,
			case_sensitive: palette.searchCaseSensitive,
			whole_word: palette.searchWholeWord,
			regex: palette.searchRegex,
			include_glob: include.length === 0 ? null : include,
			max_matches: 200,
		});
		palette.contentResults = result.hits;
		palette.contentTruncated = result.truncated;
	} catch (err) {
		workspace.flash(`Search failed: ${formatError(err)}`);
		palette.contentResults = [];
	} finally {
		palette.loading = false;
	}
}

/**
 * Walk the active folder and apply `palette.replaceText` to every
 * match of `palette.query` (with the same case / whole-word / regex
 * / include-glob toggles as the preview). Two gates run before the
 * write loop kicks off:
 *
 *   1. Confirm with the user. The match count is whatever the
 *      preview last showed; we tell the user it's a lower bound
 *      (the search list is capped at 200, the replace is not) so a
 *      "Replace 200 matches" prompt doesn't lull them into thinking
 *      that's an upper bound.
 *   2. Flag any open buffer that's dirty *and* matches the include
 *      filter — replacing on disk while the user has unsaved edits
 *      means the next save would silently revert the refactor, the
 *      single worst failure mode for this feature. We surface it as
 *      a separate confirm so the user can save first if they want.
 *
 * On success the file watcher pipeline reloads open buffers; we
 * just close the palette and flash a summary.
 */
export async function runContentReplace() {
	if (!workspace.workspace) {
		return;
	}
	const query = palette.query.trim();
	if (query.length === 0) {
		workspace.flash('Enter something to search for before replacing.');
		return;
	}
	if (palette.replaceText === palette.query) {
		workspace.flash('Replacement is identical to the query — nothing to do.');
		return;
	}

	const include = palette.searchInclude.trim();
	const previewCount = palette.contentResults.length;
	const lowerBoundNote = palette.contentTruncated ? ' (or more — preview was capped)' : '';
	const previewSuffix =
		previewCount > 0 ? ` Preview matched ${previewCount} line${previewCount === 1 ? '' : 's'}${lowerBoundNote}.` : '';
	const includeNote = include.length === 0 ? '' : `\nScope: ${include}`;
	const ok = await confirm(
		`Replace every "${palette.query}" with "${palette.replaceText}" across the workspace?${previewSuffix}${includeNote}`,
		{ title: 'Replace in Files', okLabel: 'Replace All', cancelLabel: 'Cancel', kind: 'warning' },
	);
	if (!ok) {
		return;
	}

	const dirtyHits = workspace.openFiles.filter(
		(f) => f.isDirty && palette.contentResults.some((h) => h.path === f.path),
	);
	if (dirtyHits.length > 0) {
		const list = dirtyHits
			.map((f) => f.path)
			.slice(0, 5)
			.join(', ');
		const extra = dirtyHits.length > 5 ? `, +${dirtyHits.length - 5} more` : '';
		const proceed = await confirm(
			`${dirtyHits.length} open file${dirtyHits.length === 1 ? ' has' : 's have'} unsaved changes (${list}${extra}). Replacing on disk now will discard them on the next reload. Continue?`,
			{ title: 'Unsaved changes', okLabel: 'Replace anyway', cancelLabel: 'Cancel', kind: 'warning' },
		);
		if (!proceed) {
			return;
		}
	}

	palette.replaceRunning = true;
	try {
		const result = await ipc.search.replaceContent({
			query: palette.query,
			replacement: palette.replaceText,
			case_sensitive: palette.searchCaseSensitive,
			whole_word: palette.searchWholeWord,
			regex: palette.searchRegex,
			include_glob: include.length === 0 ? null : include,
		});
		const filePlural = result.files_changed === 1 ? 'file' : 'files';
		const matchPlural = result.replacements === 1 ? 'replacement' : 'replacements';
		const summary = `${result.replacements} ${matchPlural} across ${result.files_changed} ${filePlural}.`;
		const firstErr = result.errors[0];
		if (firstErr) {
			workspace.flash(
				`Replace done: ${summary} ${result.errors.length} error${result.errors.length === 1 ? '' : 's'} (${firstErr.path}: ${firstErr.message}).`,
			);
		} else {
			workspace.flash(`Replace done: ${summary}`);
		}
		palette.hide();
	} catch (err) {
		workspace.flash(`Replace failed: ${formatError(err)}`);
	} finally {
		palette.replaceRunning = false;
	}
}
