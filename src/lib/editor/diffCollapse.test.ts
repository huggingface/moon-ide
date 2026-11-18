import { describe, expect, it } from 'vitest';

import { javascript } from '@codemirror/lang-javascript';
import { ensureSyntaxTree, StreamLanguage } from '@codemirror/language';
import { properties } from '@codemirror/legacy-modes/mode/properties';
import { unifiedMergeView } from '@codemirror/merge';
import { EditorState, Text, type Extension } from '@codemirror/state';
import { Tree } from '@lezer/common';

import {
	blockReveal,
	changedLinesOf,
	collapsedLines,
	defaultCollapsed,
	diffCollapse,
	mapLineRange,
	planBar,
	planCollapsed,
	revealLines,
	unchangedRuns,
	type ChangedLines,
	type LineRange,
} from './diffCollapse';
import type { EnclosingBlock } from './enclosingScope';

const lines = (count: number, prefix = 'line') => Array.from({ length: count }, (_, i) => `${prefix} ${i + 1}`);

describe('changedLinesOf', () => {
	const doc = Text.of(['a', 'b', 'c', 'd']);

	it('covers the lines a chunk spans', () => {
		// `to` is the start of the line after the chunk.
		expect(changedLinesOf(doc, 2, 6)).toEqual({ first: 2, last: 3 });
	});

	it('reads an empty side as an insertion point before a line', () => {
		expect(changedLinesOf(doc, 4, 4)).toEqual({ first: 3, last: 2 });
	});

	it('handles a chunk that runs past the end of the document', () => {
		expect(changedLinesOf(doc, 6, doc.length + 1)).toEqual({ first: 4, last: 4 });
		expect(changedLinesOf(doc, doc.length + 1, doc.length + 1)).toEqual({ first: 5, last: 4 });
	});
});

describe('defaultCollapsed', () => {
	it('keeps three lines of context on each side of a change', () => {
		const changes: ChangedLines[] = [
			{ first: 20, last: 21 },
			{ first: 60, last: 60 },
		];
		expect(defaultCollapsed(changes, 100)).toEqual([
			{ from: 1, to: 16 },
			{ from: 25, to: 56 },
			{ from: 64, to: 100 },
		]);
	});

	it('leaves a run open when fewer than five lines would collapse', () => {
		// 10 unchanged lines between the changes, 6 of them context.
		expect(
			defaultCollapsed(
				[
					{ first: 1, last: 1 },
					{ first: 12, last: 12 },
				],
				12,
			),
		).toEqual([]);
		// One more line and the middle five collapse.
		expect(
			defaultCollapsed(
				[
					{ first: 1, last: 1 },
					{ first: 13, last: 13 },
				],
				13,
			),
		).toEqual([{ from: 5, to: 9 }]);
	});

	it('gives the same shape on the side where a chunk is empty', () => {
		// A pure insertion before line 30 of this side.
		expect(defaultCollapsed([{ first: 30, last: 29 }], 60)).toEqual([
			{ from: 1, to: 26 },
			{ from: 33, to: 60 },
		]);
	});

	it('collapses nothing in a file with no room', () => {
		expect(defaultCollapsed([{ first: 1, last: 4 }], 4)).toEqual([]);
		expect(defaultCollapsed([], 3)).toEqual([]);
	});
});

describe('revealLines', () => {
	const ranges: LineRange[] = [
		{ from: 1, to: 16 },
		{ from: 25, to: 56 },
	];

	it('keeps the remainder collapsed as its own range', () => {
		expect(revealLines(ranges, { from: 40, to: 56 })).toEqual([
			{ from: 1, to: 16 },
			{ from: 25, to: 39 },
		]);
	});

	it('can leave a remainder on both sides', () => {
		expect(revealLines(ranges, { from: 35, to: 40 })).toEqual([
			{ from: 1, to: 16 },
			{ from: 25, to: 34 },
			{ from: 41, to: 56 },
		]);
	});

	it('opens a remainder that is under the minimum size', () => {
		expect(revealLines(ranges, { from: 29, to: 56 })).toEqual([{ from: 1, to: 16 }]);
		expect(revealLines(ranges, { from: 30, to: 56 })).toEqual([
			{ from: 1, to: 16 },
			{ from: 25, to: 29 },
		]);
	});

	it('ignores a reveal that touches nothing', () => {
		expect(revealLines(ranges, { from: 17, to: 24 })).toEqual(ranges);
	});
});

describe('unchangedRuns and mapLineRange', () => {
	// Base: 100 lines. Working: lines 20-21 rewritten as three lines,
	// two lines inserted before base line 50, base lines 80-82 removed.
	const a: ChangedLines[] = [
		{ first: 20, last: 21 },
		{ first: 50, last: 49 },
		{ first: 80, last: 82 },
	];
	const b: ChangedLines[] = [
		{ first: 20, last: 22 },
		{ first: 51, last: 52 },
		{ first: 83, last: 82 },
	];
	const runs = unchangedRuns(a, b, 100, 100);

	it('pairs up the unchanged runs of both sides', () => {
		expect(runs).toEqual([
			{ a: 1, b: 1, length: 19 },
			{ a: 22, b: 23, length: 28 },
			{ a: 50, b: 53, length: 30 },
			{ a: 83, b: 83, length: 18 },
		]);
	});

	it('maps a range through the offset of its run, both ways', () => {
		expect(mapLineRange({ from: 25, to: 46 }, runs, 'a')).toEqual({ from: 26, to: 47 });
		expect(mapLineRange({ from: 26, to: 47 }, runs, 'b')).toEqual({ from: 25, to: 46 });
		expect(mapLineRange({ from: 56, to: 79 }, runs, 'b')).toEqual({ from: 53, to: 76 });
	});

	it('has no counterpart for a range that leaves its run', () => {
		expect(mapLineRange({ from: 15, to: 25 }, runs, 'a')).toBeNull();
		expect(mapLineRange({ from: 20, to: 21 }, runs, 'a')).toBeNull();
	});

	it('gives both sides the same default collapse', () => {
		const collapsedA = defaultCollapsed(a, 100);
		const collapsedB = defaultCollapsed(b, 100);
		expect(collapsedA.map((range) => mapLineRange(range, runs, 'a'))).toEqual(collapsedB);
	});
});

describe('blockReveal', () => {
	const range = { from: 25, to: 120 };

	it('reveals from the block start down to the code below', () => {
		expect(blockReveal(range, { from: 90, to: 140 }, 'below')).toEqual({
			reveal: { from: 90, to: 120 },
			kind: 'block',
		});
	});

	it('reveals from the code above down to the block end', () => {
		expect(blockReveal(range, { from: 10, to: 41 }, 'above')).toEqual({
			reveal: { from: 25, to: 41 },
			kind: 'block',
		});
	});

	it('steps by 20 lines when the block would open everything', () => {
		expect(blockReveal(range, { from: 5, to: 300 }, 'below')).toEqual({
			reveal: { from: 101, to: 120 },
			kind: 'lines',
		});
		expect(blockReveal(range, { from: 5, to: 300 }, 'above')).toEqual({
			reveal: { from: 25, to: 44 },
			kind: 'lines',
		});
	});

	it('steps by 20 lines when no block is known', () => {
		expect(blockReveal(range, null, 'below')?.kind).toBe('lines');
	});

	it('offers nothing when even a step would open everything', () => {
		expect(blockReveal({ from: 25, to: 44 }, null, 'below')).toBeNull();
		expect(blockReveal({ from: 25, to: 44 }, { from: 1, to: 300 }, 'above')).toBeNull();
	});

	it('still reveals a block that starts on the last collapsed line', () => {
		expect(blockReveal(range, { from: 120, to: 140 }, 'below')?.reveal).toEqual({ from: 120, to: 120 });
	});
});

describe('planBar', () => {
	// One function over lines 90-140 with a change at 124-125; another
	// over 10-41 with a change at 20-21. The bar hides 25-120.
	const range = { from: 25, to: 120 };
	const changes: ChangedLines[] = [
		{ first: 20, last: 21 },
		{ first: 124, last: 125 },
	];
	const fnAbove: EnclosingBlock = { from: 10, to: 41, label: 'fn above()' };
	const fnBelow: EnclosingBlock = { from: 90, to: 140, label: 'fn below()' };
	const outer: EnclosingBlock = { from: 5, to: 300, label: 'mod outer' };
	const asked: [LineRange, string][] = [];
	const blocksAt = (target: LineRange, direction: 'above' | 'below') => {
		asked.push([target, direction]);
		return direction === 'below' ? [fnBelow, outer] : [fnAbove, outer];
	};

	it('names the block below and offers both partial expansions', () => {
		asked.length = 0;
		expect(planBar(range, changes, 400, blocksAt)).toEqual({
			symbol: 'fn below()',
			below: { reveal: { from: 90, to: 120 }, kind: 'block' },
			above: { reveal: { from: 25, to: 41 }, kind: 'block' },
		});
		// The blocks asked about are those of the adjacent changes.
		expect(asked).toEqual([
			[{ from: 124, to: 125 }, 'below'],
			[{ from: 20, to: 21 }, 'above'],
		]);
	});

	it('skips a block whose first line is already visible', () => {
		// The change sits in a function that starts inside the context.
		const visible: EnclosingBlock = { from: 122, to: 130, label: 'fn visible()' };
		const plan = planBar(range, changes, 400, (_target, direction) =>
			direction === 'below' ? [visible, fnBelow] : [],
		);
		expect(plan.symbol).toBe('fn below()');
		expect(plan.below?.reveal).toEqual({ from: 90, to: 120 });
	});

	it('asks about the visible neighbour line when the change is further away', () => {
		asked.length = 0;
		planBar({ from: 25, to: 80 }, changes, 400, blocksAt);
		expect(asked).toEqual([
			[{ from: 81, to: 81 }, 'below'],
			[{ from: 20, to: 21 }, 'above'],
		]);
	});

	it('falls back to the visible line when no block encloses the change', () => {
		// The change spans the end of one function and the start of
		// the next; the context line above it is still inside `fnBelow`.
		const plan = planBar(range, changes, 400, (target, direction) =>
			direction === 'below' && target.from === 121 ? [fnBelow] : [],
		);
		expect(plan.symbol).toBe('fn below()');
		expect(plan.below).toEqual({ reveal: { from: 90, to: 120 }, kind: 'block' });
	});

	it('uses the visible line where the change is empty on this side', () => {
		asked.length = 0;
		// The other side inserted lines before line 124 of this one.
		planBar(range, [{ first: 124, last: 123 }], 400, blocksAt);
		expect(asked).toEqual([
			[{ from: 121, to: 121 }, 'below'],
			[{ from: 24, to: 24 }, 'above'],
		]);
	});

	it('offers nothing towards the edge of the document', () => {
		const top = planBar({ from: 1, to: 16 }, changes, 400, () => []);
		expect(top.above).toBeNull();
		const bottom = planBar({ from: 129, to: 400 }, changes, 400, () => []);
		expect(bottom.below).toBeNull();
		expect(bottom.symbol).toBeNull();
	});
});

describe('diffCollapse in an editor', () => {
	const base = lines(60);
	const working = [...base];
	working[29] = 'line 30 changed';
	const create = (extra: Extension = []) =>
		EditorState.create({
			doc: working.join('\n'),
			extensions: [diffCollapse(), unifiedMergeView({ original: base.join('\n') }), extra],
		});

	it('collapses the unchanged runs around the change', () => {
		expect(collapsedLines(create())).toEqual([
			{ from: 1, to: 26 },
			{ from: 34, to: 60 },
		]);
	});

	it('maps collapsed ranges through an edit elsewhere', () => {
		const state = create();
		const line = state.doc.line(28);
		const next = state.update({ changes: { from: line.to, insert: '\nnew\nnew' } }).state;
		expect(collapsedLines(next)).toEqual([
			{ from: 1, to: 26 },
			{ from: 36, to: 62 },
		]);
	});

	it('keeps a range when a line is added right above it', () => {
		const state = create();
		const next = state.update({ changes: { from: state.doc.line(33).to, insert: '\nnew' } }).state;
		expect(collapsedLines(next)).toEqual([
			{ from: 1, to: 26 },
			{ from: 35, to: 61 },
		]);
	});

	it('opens a range the edit reaches into', () => {
		const state = create();
		const next = state.update({ changes: { from: state.doc.line(40).from, insert: 'x' } }).state;
		expect(collapsedLines(next)).toEqual([{ from: 1, to: 26 }]);
	});

	it('opens a range when its edge line is joined to a visible one', () => {
		const state = create();
		const edge = state.doc.line(34).from;
		const next = state.update({ changes: { from: edge - 1, to: edge } }).state;
		expect(collapsedLines(next)).toEqual([{ from: 1, to: 26 }]);
	});

	it('starts over when the document is replaced wholesale', () => {
		const state = create();
		const replaced = [...base];
		replaced[9] = 'line 10 changed';
		const next = state.update({
			changes: { from: 0, to: state.doc.length, insert: replaced.join('\n') },
		}).state;
		expect(collapsedLines(next)).toEqual([
			{ from: 1, to: 6 },
			{ from: 14, to: 60 },
		]);
	});
});

describe('planCollapsed', () => {
	const body = (name: string) => [`function ${name}() {`, ...lines(30, `\t${name}`), '}', ''];
	// 33 lines each: `first` closes on line 32, `second` runs 34-65.
	const base = [...body('first'), ...body('second'), ...body('third')].slice(0, -1);
	const working = [...base];
	working[49] = '\tsecond changed';
	const create = (lang: Extension) =>
		EditorState.create({
			doc: working.join('\n'),
			extensions: [diffCollapse(), lang, unifiedMergeView({ original: base.join('\n') })],
		});
	const expected = [
		{
			range: { from: 1, to: 46 },
			plan: { symbol: 'function second()', below: { reveal: { from: 34, to: 46 }, kind: 'block' }, above: null },
		},
		{
			range: { from: 54, to: 98 },
			plan: { symbol: null, below: null, above: { reveal: { from: 54, to: 65 }, kind: 'block' } },
		},
	];

	it('finds the enclosing function in the syntax tree', () => {
		const state = create(javascript());
		const tree = ensureSyntaxTree(state, state.doc.length, 5000);
		expect(tree).not.toBeNull();
		expect(planCollapsed(state, tree ?? Tree.empty)).toEqual(expected);
	});

	it('waits for the parse rather than guess from a partial tree', () => {
		const state = create(javascript());
		const plans = planCollapsed(state, Tree.empty);
		expect(plans.map((entry) => entry.plan.symbol)).toEqual([null, null]);
		// Without a block the bar still offers a fixed step.
		expect(plans[0]?.plan.below).toEqual({ reveal: { from: 27, to: 46 }, kind: 'lines' });
	});

	it('falls back to indentation where the language has no real tree', () => {
		const state = create(StreamLanguage.define(properties));
		expect(planCollapsed(state, Tree.empty)).toEqual(expected);
	});

	it('falls back to a fixed step in plain text with no definitions', () => {
		const plain = lines(80);
		const changed = [...plain];
		changed[59] = 'changed';
		const state = EditorState.create({
			doc: changed.join('\n'),
			extensions: [diffCollapse(), unifiedMergeView({ original: plain.join('\n') })],
		});
		expect(planCollapsed(state, Tree.empty)[0]).toEqual({
			range: { from: 1, to: 56 },
			plan: { symbol: null, below: { reveal: { from: 37, to: 56 }, kind: 'lines' }, above: null },
		});
	});
});
