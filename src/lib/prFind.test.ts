import { describe, expect, it } from 'vitest';
import { findInFiles, removedLines } from './prFind';

describe('find across a pull request', () => {
	const base = 'keep\nconst resetJitter = 1;\nkeep too\n';
	const after = 'keep\nconst resetJitterInSeconds = 1;\nkeep too\nuse(resetJitterInSeconds)\n';
	// One change: line 2 of the old text became line 2 of the new.
	const removed = removedLines(base, after, [{ fromA: 5, toA: 28, fromB: 5 }]);

	it('knows which old lines were removed, and where they sat', () => {
		expect([...removed]).toEqual([[2, { anchor: 2, index: 0 }]]);
	});

	it('searches the new text whole and the old only where removed', () => {
		const files = [{ path: 'a.ts', base, after, removed }];
		expect(findInFiles(files, 'resetjitter').map((hit) => [hit.side, hit.line, hit.from])).toEqual([
			['a', 2, 6],
			['b', 2, 6],
			['b', 4, 4],
		]);
		expect(findInFiles(files, 'keep')).toHaveLength(2);
		expect(findInFiles(files, 'ResetJitter', true)).toEqual([]);
		expect(findInFiles(files, '')).toEqual([]);
	});

	it('lists files in their order', () => {
		const files = [
			{ path: 'b.ts', base: '', after: 'x x', removed: new Map() },
			{ path: 'a.ts', base: '', after: 'x', removed: new Map() },
		];
		expect(findInFiles(files, 'x').map((hit) => `${hit.path}:${hit.from}`)).toEqual(['b.ts:0', 'b.ts:2', 'a.ts:0']);
	});
});
