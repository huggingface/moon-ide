import { describe, expect, it } from 'vitest';
import { attentionCounts, LARGE_FILE_LINES, LARGE_PR_FILES, nextUnviewed, startsCollapsed } from './prFiles';
import type { PrDiffFile } from './protocol';

function file(path: string, over: Partial<PrDiffFile> = {}): PrDiffFile {
	return { path, status: 'modified', additions: 3, deletions: 1, ...over };
}

describe('startsCollapsed', () => {
	it('leaves an ordinary file of a small pull request open', () => {
		expect(startsCollapsed(file('src/a.ts'), 3)).toBe(false);
		expect(startsCollapsed(file('src/a.ts'), LARGE_PR_FILES)).toBe(false);
	});

	it('collapses everything in a large pull request', () => {
		expect(startsCollapsed(file('src/a.ts'), LARGE_PR_FILES + 1)).toBe(true);
	});

	it('collapses deleted, generated, binary and very large files', () => {
		expect(startsCollapsed(file('src/a.ts', { status: 'deleted' }), 2)).toBe(true);
		expect(startsCollapsed(file('Cargo.lock'), 2)).toBe(true);
		expect(startsCollapsed(file('web/bun.lock'), 2)).toBe(true);
		expect(startsCollapsed(file('src/__snapshots__/a.test.ts.snap'), 2)).toBe(true);
		expect(startsCollapsed(file('logo.png', { additions: null, deletions: null }), 2)).toBe(true);
		expect(startsCollapsed(file('src/a.ts', { additions: LARGE_FILE_LINES, deletions: 1 }), 2)).toBe(true);
		expect(startsCollapsed(file('src/lockfile.ts'), 2)).toBe(false);
	});
});

describe('nextUnviewed', () => {
	const files = [file('a'), file('b'), file('c'), file('d')];

	it('moves forward past viewed files', () => {
		expect(nextUnviewed(files, { a: true, b: true }, 'a')).toBe('c');
	});

	it('wraps round to files before the current one', () => {
		expect(nextUnviewed(files, { c: true, d: true }, 'd')).toBe('a');
	});

	it('is null when everything is viewed', () => {
		expect(nextUnviewed(files, { a: true, b: true, c: true, d: true }, 'b')).toBeNull();
		expect(nextUnviewed([file('a')], {}, 'a')).toBeNull();
	});
});

describe('attentionCounts', () => {
	it('counts unresolved threads and notes per path', () => {
		const pr = {
			threads: [
				{ path: 'a', resolved: false },
				{ path: 'a', resolved: true },
				{ path: 'b', resolved: false },
			],
			minorNotes: [{ path: 'a' }],
		};
		expect([...attentionCounts(pr)]).toEqual([
			['a', 2],
			['b', 1],
		]);
	});
});
