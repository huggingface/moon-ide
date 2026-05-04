import { confirm, open } from '@tauri-apps/plugin-dialog';
import { workspace } from './state.svelte';
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

export type PaletteMode = 'commands' | 'files' | 'search';

class PaletteState {
	open = $state(false);
	mode = $state<PaletteMode>('commands');
	query = $state('');

	fileResults = $state<FileSearchResult[]>([]);
	contentResults = $state<ContentSearchHit[]>([]);
	contentTruncated = $state(false);
	loading = $state(false);

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
}

export const palette = new PaletteState();

export const builtInCommands: Command[] = [
	{
		id: 'workspace.openFolder',
		title: 'Open Folder…',
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
		run: () => palette.show('search'),
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
		id: 'chat.togglePanel',
		// Wording flips with panel state — same diagnostic value as
		// the theme toggle, and means the user knows which way the
		// command goes before clicking.
		title: () => (slack.panelVisible ? 'Chat: Hide Panel' : 'Chat: Show Panel'),
		shortcut: 'Ctrl+L',
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
		id: 'git.viewDiff',
		title: 'Git: View Diff',
		// Visible only when the active file is a **modified**
		// working-tree change. Deleted files already render in diff
		// view on their own, and untracked / added / ignored files
		// have no `HEAD` side worth rendering. Diff tabs themselves
		// hide the command — there's nothing to toggle *to* from
		// within the diff.
		visible: () => {
			const path = workspace.activePath;
			if (path === null) {
				return false;
			}
			const file = workspace.openFiles.find((f) => f.path === path);
			if (!file || file.isDeleted || file.isDiffTab) {
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
			void workspace.openDiffTab(path);
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
		const result = await ipc.search.content({ query, max_matches: 200 });
		palette.contentResults = result.hits;
		palette.contentTruncated = result.truncated;
	} catch (err) {
		workspace.flash(`Search failed: ${formatError(err)}`);
		palette.contentResults = [];
	} finally {
		palette.loading = false;
	}
}
