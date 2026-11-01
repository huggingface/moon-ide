import { describe, expect, it } from 'vitest';

import type { CoderRow } from './coder.svelte';
import { groupToolRows } from './toolGroups';

let seq = 0;
function tool(name: string, args: Record<string, unknown>, result: unknown = {}): CoderRow {
	seq++;
	return {
		kind: 'tool',
		id: `t${seq}`,
		name,
		args,
		result,
		hasResult: true,
		isError: false,
		startedAt: 0,
		durationMs: 10,
		bgStatus: null,
	};
}

function assistant(text: string, thinking = ''): CoderRow {
	seq++;
	return { kind: 'assistant', id: `a${seq}`, text, thinking, thinkingOpen: false, createdAt: 0 };
}

describe('groupToolRows', () => {
	it('folds a file-tool run across quiet assistant rows, with diff totals', () => {
		const rows = [
			tool('read_file', { path: 'src/a.ts' }),
			assistant('', 'thinking about it'),
			tool('edit_file', { path: 'src/a.ts', find: 'keep\nold\nkeep2', replace: 'keep\nnew1\nnew2\nkeep2' }),
			tool('write_file', { path: 'src/b.ts', content: 'x\ny\n' }),
			assistant('Done.'),
		];
		const items = groupToolRows(rows);
		expect(items).toHaveLength(2);
		const group = items[0];
		if (group?.kind !== 'group') {
			throw new Error('expected a group');
		}
		expect(group.group).toBe('files');
		expect(group.rows).toHaveLength(4);
		expect(group.summary.calls).toBe(3);
		expect(group.summary.added).toBe(4);
		expect(group.summary.removed).toBe(1);
		expect(group.summary.files.map((f) => [f.path, f.added, f.removed, f.read])).toEqual([
			['src/a.ts', 2, 1, true],
			['src/b.ts', 2, 0, false],
		]);
	});

	it('keeps bash and file runs apart and leaves single calls alone', () => {
		const rows = [
			tool('bash', { cmd: 'ls' }, { exit_code: 0 }),
			tool('bash', { cmd: 'false' }, { exit_code: 1 }),
			tool('read_file', { path: 'a' }),
			assistant('text breaks the run'),
			tool('edit_file', { path: 'a', find: 'a', replace: 'b' }),
		];
		const items = groupToolRows(rows);
		expect(items.map((i) => (i.kind === 'group' ? `group:${i.group}` : i.row.kind))).toEqual([
			'group:bash',
			'tool',
			'assistant',
			'tool',
		]);
		const bash = items[0];
		expect(bash?.kind === 'group' && bash.summary.failedCommands).toBe(1);
	});
});
