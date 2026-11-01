import type { CoderRow } from './coder.svelte';

type ToolRow = Extract<CoderRow, { kind: 'tool' }>;

/** Which run of tool calls a row can be folded into. File tools share
 *  one group (a read-edit-edit streak is one piece of work); `bash`
 *  runs group on their own. */
export type ToolGroupKind = 'files' | 'bash';

const GROUP_OF: Record<string, ToolGroupKind> = {
	read_file: 'files',
	edit_file: 'files',
	write_file: 'files',
	bash: 'bash',
};

/** Smallest run worth folding; a lone call stays a plain row. */
const MIN_GROUP = 2;

export type FileTouch = {
	path: string;
	added: number;
	removed: number;
	edited: boolean;
	written: boolean;
	read: boolean;
};

export type ToolGroupSummary = {
	calls: number;
	running: number;
	errors: number;
	/** Settled calls' durations, summed. */
	durationMs: number;
	/** `files` groups: one entry per path, in first-touch order. */
	files: FileTouch[];
	added: number;
	removed: number;
	/** `bash` groups: settled foreground commands that exited non-zero. */
	failedCommands: number;
};

export type TranscriptItem =
	| { kind: 'row'; row: CoderRow }
	| { kind: 'group'; id: string; group: ToolGroupKind; rows: CoderRow[]; summary: ToolGroupSummary };

function groupOf(row: CoderRow): ToolGroupKind | null {
	if (row.kind !== 'tool') {
		return null;
	}
	return GROUP_OF[row.name] ?? null;
}

/** Assistant rows with no visible text (tool-only turns, or reasoning
 *  between calls) don't break a run; they fold into it and show again
 *  when the group expands. */
function isQuietAssistant(row: CoderRow): boolean {
	return row.kind === 'assistant' && row.text.trim().length === 0;
}

/**
 * Fold runs of at least two same-kind tool calls (see
 * [`ToolGroupKind`]) into one group item; everything else passes
 * through as single rows. A group's id is its first row's, so it keeps
 * its identity (and expansion state) while a live run grows.
 */
export function groupToolRows(rows: CoderRow[]): TranscriptItem[] {
	const out: TranscriptItem[] = [];
	let i = 0;
	while (i < rows.length) {
		const row = rows[i];
		if (row === undefined) {
			break;
		}
		const kind = groupOf(row);
		if (kind === null) {
			out.push({ kind: 'row', row });
			i++;
			continue;
		}
		let calls = 0;
		let lastCall = i;
		for (const next of rows.slice(i)) {
			if (groupOf(next) === kind) {
				calls++;
				lastCall = rows.indexOf(next, lastCall);
				continue;
			}
			if (!isQuietAssistant(next)) {
				break;
			}
		}
		if (calls < MIN_GROUP) {
			out.push({ kind: 'row', row });
			i++;
			continue;
		}
		const members = rows.slice(i, lastCall + 1);
		out.push({ kind: 'group', id: row.id, group: kind, rows: members, summary: summarize(members) });
		i = lastCall + 1;
	}
	return out;
}

function summarize(rows: CoderRow[]): ToolGroupSummary {
	const summary: ToolGroupSummary = {
		calls: 0,
		running: 0,
		errors: 0,
		durationMs: 0,
		files: [],
		added: 0,
		removed: 0,
		failedCommands: 0,
	};
	const byPath = new Map<string, FileTouch>();
	for (const row of rows) {
		if (row.kind !== 'tool') {
			continue;
		}
		summary.calls++;
		if (!row.hasResult) {
			summary.running++;
		} else {
			summary.durationMs += row.durationMs ?? 0;
		}
		if (row.isError) {
			summary.errors++;
		}
		if (row.name === 'bash') {
			if (row.hasResult && !row.isError && exitCode(row) !== 0 && exitCode(row) !== null) {
				summary.failedCommands++;
			}
			continue;
		}
		const path = stringArg(row.args, 'path');
		if (path === null) {
			continue;
		}
		let touch = byPath.get(path);
		if (touch === undefined) {
			touch = { path, added: 0, removed: 0, edited: false, written: false, read: false };
			byPath.set(path, touch);
			summary.files.push(touch);
		}
		if (row.name === 'read_file') {
			touch.read = true;
			continue;
		}
		// Failed and still-running writes haven't changed anything yet.
		if (row.isError || !row.hasResult) {
			continue;
		}
		const delta =
			row.name === 'edit_file'
				? editDelta(row)
				: { added: lineCount(stringArg(row.args, 'content') ?? ''), removed: 0 };
		touch.added += delta.added;
		touch.removed += delta.removed;
		summary.added += delta.added;
		summary.removed += delta.removed;
		if (row.name === 'edit_file') {
			touch.edited = true;
		} else {
			touch.written = true;
		}
	}
	return summary;
}

function field(value: unknown, key: string): unknown {
	if (typeof value !== 'object' || value === null) {
		return undefined;
	}
	const entry: unknown = Reflect.get(value, key);
	return entry;
}

function stringArg(args: unknown, key: string): string | null {
	const value = field(args, key);
	return typeof value === 'string' ? value : null;
}

function exitCode(row: ToolRow): number | null {
	const code = field(row.result, 'exit_code');
	return typeof code === 'number' ? code : null;
}

function toLines(text: string): string[] {
	return text.length === 0 ? [] : text.split('\n');
}

export function lineCount(text: string): number {
	if (text.length === 0) {
		return 0;
	}
	return text.split('\n').length - (text.endsWith('\n') ? 1 : 0);
}

/** Lines `find` → `replace` changes, ignoring the unchanged lines the
 *  model includes around an edit as anchoring context. */
export function editDelta(row: ToolRow): { added: number; removed: number } {
	const before = toLines(stringArg(row.args, 'find') ?? '');
	const after = toLines(stringArg(row.args, 'replace') ?? '');
	let start = 0;
	while (start < before.length && start < after.length && before[start] === after[start]) {
		start++;
	}
	let end = 0;
	while (
		end < before.length - start &&
		end < after.length - start &&
		before[before.length - 1 - end] === after[after.length - 1 - end]
	) {
		end++;
	}
	return { added: after.length - start - end, removed: before.length - start - end };
}
