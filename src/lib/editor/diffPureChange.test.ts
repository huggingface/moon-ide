import { describe, expect, it } from 'vitest';

import type { Change } from '@codemirror/merge';

import { lineFullyCovered } from './diffPureChange';

// Build a Change with chunk-relative offsets. Order matters: the
// helper assumes changes are sorted by `fromA` / `fromB`, which
// the merge package guarantees in production.
function ch(fromA: number, toA: number, fromB: number, toB: number): Change {
	return { fromA, toA, fromB, toB } as Change;
}

describe('lineFullyCovered (B side)', () => {
	it('treats an empty line as covered', () => {
		const line = { from: 10, to: 10 };
		expect(lineFullyCovered(line, [], 10, false)).toBe(true);
	});

	it('returns true when the entire line falls inside one Change', () => {
		// Chunk starts at doc offset 100. Line covers doc [120, 130].
		// One change spans the full chunk on B: [0, 60).
		const line = { from: 120, to: 130 };
		const changes = [ch(0, 0, 0, 60)];
		expect(lineFullyCovered(line, changes, 100, false)).toBe(true);
	});

	it('returns false when the line has a common-substring gap before the change', () => {
		// Line covers chunk-relative [0, 20]. A change starts at 5,
		// so [0, 5) is shared with A on this line — keep the
		// per-character highlight.
		const line = { from: 0, to: 20 };
		const changes = [ch(0, 0, 5, 20)];
		expect(lineFullyCovered(line, changes, 0, false)).toBe(false);
	});

	it('returns false when the line has a gap between two changes', () => {
		// [0, 5) covered by first change, [5, 7) common, [7, 20)
		// covered by second change. The hole means a surviving
		// substring sits in the middle of the line.
		const line = { from: 0, to: 20 };
		const changes = [ch(0, 0, 0, 5), ch(0, 0, 7, 20)];
		expect(lineFullyCovered(line, changes, 0, false)).toBe(false);
	});

	it('returns false when the line ends past the last change', () => {
		const line = { from: 0, to: 20 };
		const changes = [ch(0, 0, 0, 15)];
		expect(lineFullyCovered(line, changes, 0, false)).toBe(false);
	});

	it('returns true for a line in the middle of a multi-line change', () => {
		// The pattern the user hit: one Change spans a 7-line
		// insertion on B. The 5 middle lines should be flagged as
		// fully-covered (no common substring with A), so the
		// per-character highlight is stripped on them.
		// Chunk starts at 100. Line 3 of the chunk covers [140, 150]
		// while the change covers [0, 200).
		const line = { from: 140, to: 150 };
		const changes = [ch(0, 50, 0, 200)];
		expect(lineFullyCovered(line, changes, 100, false)).toBe(true);
	});
});

describe('lineFullyCovered (A side)', () => {
	it('uses fromA/toA when isA is true', () => {
		// The change covers [0, 30) on A but only [0, 5) on B; on
		// the A side every byte of the line is inside the change.
		const line = { from: 0, to: 30 };
		const changes = [ch(0, 30, 0, 5)];
		expect(lineFullyCovered(line, changes, 0, true)).toBe(true);
	});

	it('returns false when the A line has a leading common substring', () => {
		const line = { from: 0, to: 30 };
		const changes = [ch(10, 30, 0, 50)];
		expect(lineFullyCovered(line, changes, 0, true)).toBe(false);
	});
});
