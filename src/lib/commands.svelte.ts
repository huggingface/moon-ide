import { open } from '@tauri-apps/plugin-dialog';
import { workspace } from './state.svelte';
import { ipc } from './ipc';
import { formatError, type FileSearchResult, type ContentSearchHit } from './protocol';

export type Command = {
	id: string;
	/**
	 * Display label. Either a fixed string or a getter so commands can
	 * reflect live state (e.g. "Switch to Light Theme" flips after each
	 * toggle). The palette calls it once per render, so cheap reads only.
	 */
	title: string | (() => string);
	shortcut?: string;
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
		id: 'editor.save',
		title: 'Save File',
		shortcut: 'Ctrl+S',
		run: () => void workspace.saveActive(),
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
	{
		id: 'theme.toggle',
		// Label reflects what the click *will do*, not the current state.
		// When the palette is reopened after a click, the wording flips —
		// useful as a sanity check that the toggle actually fired.
		title: () => (workspace.theme === 'dark' ? 'Switch to Light Theme' : 'Switch to Dark Theme'),
		run: () => workspace.toggleTheme(),
	},
];

export function filterCommands(query: string): Command[] {
	const q = query.trim().toLowerCase();
	if (!q) {
		return builtInCommands;
	}
	return builtInCommands
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
