// Collapsing of unchanged runs in the diff views (review sections,
// commit / pull-request sections), replacing `@codemirror/merge`'s
// `collapseUnchanged`.
//
// Owning it buys what the package's version can't do: a bar that
// names the enclosing definition of the code after it, and expands
// *to that block* instead of to everything. In the split layout the
// two panes hold their own collapsed ranges; every operation is
// computed in one pane and carried to the other through the chunks,
// so both collapse and expand together and stay aligned.
//
// The collapsed ranges are plain line ranges inside unchanged runs.
// Editing maps them; an edit that touches one opens it, and a
// document replaced wholesale (an external write, a moved baseline)
// starts over from the default.

import { ensureSyntaxTree, language, StreamLanguage, syntaxTree } from '@codemirror/language';
import { getChunks, mergeViewSiblings } from '@codemirror/merge';
import {
	Facet,
	StateEffect,
	StateField,
	type ChangeDesc,
	type EditorState,
	type Extension,
	type Range,
	type Text,
	type Transaction,
} from '@codemirror/state';
import {
	Decoration,
	EditorView,
	GutterMarker,
	gutterWidgetClass,
	ViewPlugin,
	WidgetType,
	type DecorationSet,
	type ViewUpdate,
} from '@codemirror/view';
import type { Tree } from '@lezer/common';
import { enclosingBlocks, enclosingBlocksByIndent, type EnclosingBlock } from './enclosingScope';
import { paneMetrics } from './paneMetrics';

/** Context lines kept visible around a change (`git diff -U3`). */
export const COLLAPSE_MARGIN = 3;
/** Shorter unchanged runs stay open: a bar would save nothing. */
export const COLLAPSE_MIN_SIZE = 5;
/** Lines revealed per step when no enclosing block is known. */
export const COLLAPSE_STEP = 20;

/** 1-based inclusive line range. */
export type LineRange = { from: number; to: number };

/** The lines one chunk changes on one side. A chunk that is empty on
 *  that side (the other side's pure insertion) sits *before* line
 *  `first` and has `last === first - 1`. */
export type ChangedLines = { first: number; last: number };

/** `length` unchanged lines starting at line `a` of the base document
 *  and line `b` of the working one. */
export type UnchangedRun = { a: number; b: number; length: number };

export type BarAction = { reveal: LineRange; kind: 'block' | 'lines' };

/** What one bar offers: the enclosing definition of the code after
 *  it, and a partial expansion towards each neighbour. */
export type BarPlan = { symbol: string | null; below: BarAction | null; above: BarAction | null };

type Direction = 'above' | 'below';

export function changedLinesOf(doc: Text, from: number, to: number): ChangedLines {
	if (from >= to) {
		const first = from > doc.length ? doc.lines + 1 : doc.lineAt(from).number;
		return { first, last: first - 1 };
	}
	return { first: doc.lineAt(from).number, last: doc.lineAt(Math.min(doc.length, to - 1)).number };
}

/** The ranges collapsed before the user touches anything: every
 *  unchanged run minus `margin` lines of context next to a change,
 *  when at least `minSize` lines are left. */
export function defaultCollapsed(
	changes: readonly ChangedLines[],
	docLines: number,
	margin = COLLAPSE_MARGIN,
	minSize = COLLAPSE_MIN_SIZE,
): LineRange[] {
	const out: LineRange[] = [];
	let previousLast = 0;
	for (let i = 0; i <= changes.length; i++) {
		const next = changes[i];
		const from = i === 0 ? 1 : previousLast + 1 + margin;
		const to = next === undefined ? docLines : next.first - 1 - margin;
		if (to - from + 1 >= minSize) {
			out.push({ from, to });
		}
		if (next !== undefined) {
			previousLast = Math.max(previousLast, next.last);
		}
	}
	return out;
}

/** Open `reveal` inside `ranges`. What is left of a range on either
 *  side stays collapsed when it is still worth a bar, and opens too
 *  otherwise. */
export function revealLines(ranges: readonly LineRange[], reveal: LineRange, minSize = COLLAPSE_MIN_SIZE): LineRange[] {
	const out: LineRange[] = [];
	for (const range of ranges) {
		if (reveal.to < range.from || reveal.from > range.to) {
			out.push(range);
			continue;
		}
		const before = { from: range.from, to: reveal.from - 1 };
		const after = { from: reveal.to + 1, to: range.to };
		if (before.to - before.from + 1 >= minSize) {
			out.push(before);
		}
		if (after.to - after.from + 1 >= minSize) {
			out.push(after);
		}
	}
	return out;
}

export function unchangedRuns(
	a: readonly ChangedLines[],
	b: readonly ChangedLines[],
	linesA: number,
	linesB: number,
): UnchangedRun[] {
	const out: UnchangedRun[] = [];
	let nextA = 1;
	let nextB = 1;
	for (let i = 0; i <= a.length; i++) {
		const changeA = a[i];
		const changeB = b[i];
		const endA = changeA === undefined ? linesA + 1 : changeA.first;
		const endB = changeB === undefined ? linesB + 1 : changeB.first;
		const length = Math.min(endA - nextA, endB - nextB);
		if (length > 0) {
			out.push({ a: nextA, b: nextB, length });
		}
		if (changeA === undefined || changeB === undefined) {
			break;
		}
		nextA = changeA.last + 1;
		nextB = changeB.last + 1;
	}
	return out;
}

/** The same lines on the other side, or `null` when `range` is not
 *  wholly inside one unchanged run (it has no counterpart then). */
export function mapLineRange(range: LineRange, runs: readonly UnchangedRun[], from: 'a' | 'b'): LineRange | null {
	const to = from === 'a' ? 'b' : 'a';
	for (const run of runs) {
		if (range.from >= run[from] && range.to < run[from] + run.length) {
			const shift = run[to] - run[from];
			return { from: range.from + shift, to: range.to + shift };
		}
	}
	return null;
}

/**
 * The partial expansion of `range` towards one neighbour. `block` is
 * the enclosing definition of that neighbour: below the bar we show
 * from the block's first line down to the code that follows; above
 * it, from the code that precedes down to the block's last line.
 * When the block is unknown, or showing it would open the whole
 * range anyway, step by `step` lines instead, as long as that too
 * leaves something collapsed.
 */
export function blockReveal(
	range: LineRange,
	block: LineRange | null,
	direction: Direction,
	step = COLLAPSE_STEP,
): BarAction | null {
	if (block !== null) {
		if (direction === 'below' && block.from > range.from && block.from <= range.to) {
			return { reveal: { from: block.from, to: range.to }, kind: 'block' };
		}
		if (direction === 'above' && block.to >= range.from && block.to < range.to) {
			return { reveal: { from: range.from, to: block.to }, kind: 'block' };
		}
	}
	if (range.to - range.from + 1 <= step) {
		return null;
	}
	return direction === 'below'
		? { reveal: { from: range.to - step + 1, to: range.to }, kind: 'lines' }
		: { reveal: { from: range.from, to: range.from + step - 1 }, kind: 'lines' };
}

/**
 * What a bar's neighbour is about, most specific first: the adjacent
 * change when one starts within the context margin, then the visible
 * line right next to the bar. Empty at the document's edge.
 *
 * A change that is empty on this side (the other side's insertion)
 * has no lines here, so only the visible line counts; both panes
 * then usually land on the same block.
 */
function neighbours(
	range: LineRange,
	changes: readonly ChangedLines[],
	docLines: number,
	direction: Direction,
	margin: number,
): LineRange[] {
	const edge = direction === 'below' ? range.to + 1 : range.from - 1;
	if (edge < 1 || edge > docLines) {
		return [];
	}
	const visible = { from: edge, to: edge };
	const change =
		direction === 'below' ? changes.find((c) => c.first > range.to) : changes.findLast((c) => c.last < range.from);
	if (change === undefined || change.last < change.first) {
		return [visible];
	}
	const gap = direction === 'below' ? change.first - edge : edge - change.last;
	if (gap > margin) {
		return [visible];
	}
	return [{ from: change.first, to: Math.min(change.last, docLines) }, visible];
}

/**
 * Plan one bar. `blocksAt` answers "which definitions enclose these
 * lines, innermost first" (the syntax tree, or the indent heuristic
 * where there is none). Below the bar the block that counts is the
 * innermost one whose first line is hidden; above it, the innermost
 * one whose last line is. A change no single block encloses (it
 * spans two functions, say) falls back to the block of the visible
 * line next to the bar.
 */
export function planBar(
	range: LineRange,
	changes: readonly ChangedLines[],
	docLines: number,
	blocksAt: (target: LineRange, direction: Direction) => readonly EnclosingBlock[],
	margin = COLLAPSE_MARGIN,
	step = COLLAPSE_STEP,
): BarPlan {
	const find = (direction: Direction, hidden: (block: EnclosingBlock) => boolean) => {
		const targets = neighbours(range, changes, docLines, direction, margin);
		for (const target of targets) {
			const block = blocksAt(target, direction).find(hidden);
			if (block !== undefined) {
				return { reachable: true, block };
			}
		}
		return { reachable: targets.length > 0, block: null };
	};
	const below = find('below', (block) => block.from <= range.to);
	const above = find('above', (block) => block.to >= range.from);
	return {
		symbol: below.block?.label ?? null,
		below: below.reachable ? blockReveal(range, below.block, 'below', step) : null,
		above: above.reachable ? blockReveal(range, above.block, 'above', step) : null,
	};
}

// --- CodeMirror wiring ------------------------------------------------

/** Lines (1-based) that must stay visible because something hangs
 *  off them: a review thread, a draft comment. */
export const diffCollapsePins = Facet.define<readonly number[], readonly number[]>({
	combine: (values) => values.flat(),
	compare: (a, b) => a.length === b.length && a.every((line, i) => line === b[i]),
});

type PosRange = { from: number; to: number };

type CollapseState = {
	ranges: readonly PosRange[];
	deco: DecorationSet;
	/** The document was replaced wholesale; rebuild from the default
	 *  once the chunks for the new text arrive. */
	rebuild: boolean;
};

const setCollapsed = StateEffect.define<readonly PosRange[]>();

function toLines(doc: Text, range: PosRange): LineRange {
	return { from: doc.lineAt(range.from).number, to: doc.lineAt(range.to).number };
}

function toPositions(doc: Text, range: LineRange): PosRange {
	return { from: doc.line(range.from).from, to: doc.line(range.to).to };
}

function ownChanges(state: EditorState): ChangedLines[] {
	const info = getChunks(state);
	if (info === null) {
		return [];
	}
	const isA = info.side === 'a';
	return info.chunks.map((chunk) =>
		changedLinesOf(state.doc, isA ? chunk.fromA : chunk.fromB, isA ? chunk.toA : chunk.toB),
	);
}

function buildDefault(state: EditorState): PosRange[] {
	return defaultCollapsed(ownChanges(state), state.doc.lines).map((range) => toPositions(state.doc, range));
}

function replacesWholeDoc(tr: Transaction): boolean {
	const length = tr.startState.doc.length;
	let whole = false;
	tr.changes.iterChangedRanges((from, to) => {
		if (from === 0 && to === length) {
			whole = true;
		}
	});
	return whole;
}

/** Carry ranges through an edit; one the edit reaches into (or onto
 *  the edge of) opens rather than guess what it now covers. */
function mapRanges(ranges: readonly PosRange[], changes: ChangeDesc): readonly PosRange[] {
	if (ranges.length === 0) {
		return ranges;
	}
	const out: PosRange[] = [];
	for (const range of ranges) {
		let touched = false;
		changes.iterChangedRanges((from, to) => {
			if (from <= range.to && to >= range.from) {
				touched = true;
			}
		});
		if (!touched) {
			out.push({ from: changes.mapPos(range.from), to: changes.mapPos(range.to) });
		}
	}
	return out;
}

/** Drop ranges a re-diff moved a change into. A change that only
 *  abuts a range (a line typed right above a bar) leaves it alone. */
function dropChanged(ranges: readonly PosRange[], state: EditorState): readonly PosRange[] {
	const info = getChunks(state);
	if (info === null || ranges.length === 0) {
		return ranges;
	}
	const isA = info.side === 'a';
	const kept = ranges.filter((range) =>
		info.chunks.every((chunk) => {
			const from = isA ? chunk.fromA : chunk.fromB;
			const to = isA ? chunk.toA : chunk.toB;
			return from === to ? from <= range.from || from > range.to : from > range.to || to <= range.from;
		}),
	);
	return kept.length === ranges.length ? ranges : kept;
}

function hasRealTree(state: EditorState): boolean {
	const lang = state.facet(language);
	return lang !== null && !(lang instanceof StreamLanguage);
}

function planFor(state: EditorState, range: LineRange, tree: Tree, changes: readonly ChangedLines[]): BarPlan {
	const doc = state.doc;
	const real = hasRealTree(state);
	return planBar(range, changes, doc.lines, (target, direction) => {
		if (!real) {
			return enclosingBlocksByIndent(doc, direction === 'below' ? target.from : target.to);
		}
		const from = doc.line(target.from).from;
		const to = doc.line(target.to).to;
		// A tree that has not reached the target yet knows nothing
		// about it; the bar is planned again when the parse gets there.
		return tree.length < to ? [] : enclosingBlocks(doc, tree, from, to);
	});
}

/** `tail`: nothing follows the collapsed run, so its bar has no block
 *  to offer an explanation of. */
type BarInfo = { lines: number; plan: BarPlan; tail: boolean };

/** When an editor provides this, each bar offers to explain the code
 *  that follows it; the callback is given that block's lines. */
export const diffCollapseAsk = Facet.define<(block: LineRange) => void, ((block: LineRange) => void) | null>({
	combine: (values) => values[0] ?? null,
});

function sameAction(a: BarAction | null, b: BarAction | null): boolean {
	if (a === null || b === null) {
		return a === b;
	}
	return a.kind === b.kind && a.reveal.from === b.reveal.from && a.reveal.to === b.reveal.to;
}

function actionText(action: BarAction, direction: Direction): { text: string; title: string } {
	if (action.kind === 'block') {
		return direction === 'below'
			? {
					text: 'Block below',
					title: `Show from line ${action.reveal.from}, where the block the code below is in starts`,
				}
			: {
					text: 'Block above',
					title: `Show to line ${action.reveal.to}, where the block the code above is in ends`,
				};
	}
	const count = action.reveal.to - action.reveal.from + 1;
	return direction === 'below'
		? { text: `+${count} below`, title: `Show the ${count} lines before the code below` }
		: { text: `+${count} above`, title: `Show the ${count} lines after the code above` };
}

type BarCommand = Direction | 'all';

class CollapseBarWidget extends WidgetType {
	constructor(private readonly info: BarInfo) {
		super();
	}

	override eq(other: CollapseBarWidget): boolean {
		const a = this.info;
		const b = other.info;
		return (
			a.lines === b.lines &&
			a.tail === b.tail &&
			a.plan.symbol === b.plan.symbol &&
			sameAction(a.plan.below, b.plan.below) &&
			sameAction(a.plan.above, b.plan.above)
		);
	}

	override toDOM(view: EditorView): HTMLElement {
		const { lines, plan } = this.info;
		const root = document.createElement('div');
		root.className = 'cm-diffCollapse';
		const row = root.appendChild(document.createElement('div'));
		row.className = 'cm-diffCollapse-row';

		const button = (text: string, title: string, command: BarCommand, className: string): HTMLButtonElement => {
			const el = document.createElement('button');
			el.type = 'button';
			el.className = className;
			el.textContent = text;
			el.title = title;
			el.addEventListener('mousedown', (event) => event.preventDefault());
			el.addEventListener('click', () => runCommand(view, root, command));
			return el;
		};

		const count = row.appendChild(document.createElement('span'));
		count.className = 'cm-diffCollapse-count';
		count.textContent = `${lines} unchanged line${lines === 1 ? '' : 's'}`;

		if (plan.symbol !== null) {
			if (plan.below !== null && plan.below.kind === 'block') {
				row.append(button(plan.symbol, actionText(plan.below, 'below').title, 'below', 'cm-diffCollapse-symbol'));
			} else {
				const symbol = row.appendChild(document.createElement('span'));
				symbol.className = 'cm-diffCollapse-symbol';
				symbol.textContent = plan.symbol;
			}
		}

		const actions = row.appendChild(document.createElement('span'));
		actions.className = 'cm-diffCollapse-actions';
		const ask = view.state.facet(diffCollapseAsk);
		if (ask !== null && !this.info.tail) {
			const explain = document.createElement('button');
			explain.type = 'button';
			explain.className = 'cm-diffCollapse-btn cm-diffCollapse-ask';
			explain.textContent = 'Explain';
			explain.title = 'Ask the agent what the change below does';
			explain.addEventListener('mousedown', (event) => event.preventDefault());
			explain.addEventListener('click', () => {
				const block = blockAfter(view, root);
				if (block !== null) {
					ask(block);
				}
			});
			actions.append(explain);
		}
		if (plan.above !== null) {
			const { text, title } = actionText(plan.above, 'above');
			actions.append(button(text, title, 'above', 'cm-diffCollapse-btn'));
		}
		if (plan.below !== null) {
			const { text, title } = actionText(plan.below, 'below');
			actions.append(button(text, title, 'below', 'cm-diffCollapse-btn'));
		}
		actions.append(button('Expand all', `Show all ${lines} lines`, 'all', 'cm-diffCollapse-btn'));
		return root;
	}

	override get estimatedHeight(): number {
		return 26;
	}

	override ignoreEvent(): boolean {
		return true;
	}
}

/** Every collapsed range of `state` with what its bar offers, read
 *  off `tree` (which may be a partial parse). */
export function planCollapsed(state: EditorState, tree: Tree): { range: LineRange; plan: BarPlan }[] {
	const changes = ownChanges(state);
	return collapsedLines(state).map((range) => ({ range, plan: planFor(state, range, tree, changes) }));
}

function withDeco(state: EditorState, ranges: readonly PosRange[], rebuild: boolean): CollapseState {
	if (ranges.length === 0) {
		return { ranges, deco: Decoration.none, rebuild };
	}
	const tree = syntaxTree(state);
	const changes = ownChanges(state);
	const decorations: Range<Decoration>[] = ranges.map((range) => {
		const lines = toLines(state.doc, range);
		const info = {
			lines: lines.to - lines.from + 1,
			plan: planFor(state, lines, tree, changes),
			tail: lines.to >= state.doc.lines,
		};
		return Decoration.replace({ widget: new CollapseBarWidget(info), block: true }).range(range.from, range.to);
	});
	return { ranges, deco: Decoration.set(decorations), rebuild };
}

const NO_RANGES: readonly PosRange[] = [];

const collapseField = StateField.define<CollapseState>({
	create: (state) => withDeco(state, getChunks(state) === null ? NO_RANGES : buildDefault(state), false),
	update(value, tr) {
		let ranges = value.ranges;
		let rebuild = value.rebuild;
		let explicit = false;
		for (const effect of tr.effects) {
			if (effect.is(setCollapsed)) {
				ranges = effect.value;
				rebuild = false;
				explicit = true;
			}
		}
		const chunksBefore = getChunks(tr.startState)?.chunks ?? null;
		const chunksAfter = getChunks(tr.state)?.chunks ?? null;
		if (!explicit) {
			if (tr.docChanged) {
				if (replacesWholeDoc(tr)) {
					ranges = NO_RANGES;
					rebuild = true;
				} else {
					ranges = mapRanges(ranges, tr.changes);
				}
			}
			if (chunksAfter !== null && chunksAfter !== chunksBefore) {
				// `MergeView` adds its chunks to a pane right after the
				// state is created, so that first arrival is the initial
				// build.
				if (chunksBefore === null || rebuild) {
					ranges = buildDefault(tr.state);
					rebuild = false;
				} else {
					ranges = dropChanged(ranges, tr.state);
				}
			}
		}
		const unchanged =
			ranges === value.ranges &&
			rebuild === value.rebuild &&
			!tr.docChanged &&
			chunksAfter === chunksBefore &&
			syntaxTree(tr.startState) === syntaxTree(tr.state);
		return unchanged ? value : withDeco(tr.state, ranges, rebuild);
	},
	provide: (field) => [
		EditorView.decorations.from(field, (value) => value.deco),
		EditorView.atomicRanges.of((view) => view.state.field(field).deco),
	],
});

/** The collapsed line ranges of an editor carrying `diffCollapse()`. */
export function collapsedLines(state: EditorState): LineRange[] {
	const value = state.field(collapseField, false);
	return value === undefined ? [] : value.ranges.map((range) => toLines(state.doc, range));
}

function sameRanges(a: readonly LineRange[], b: readonly LineRange[]): boolean {
	return a.length === b.length && a.every((range, i) => range.from === b[i]?.from && range.to === b[i]?.to);
}

/** The other pane of a split diff, once it exists and collapses too. */
function siblingOf(view: EditorView): { other: EditorView; side: 'a' | 'b' } | null {
	// `MergeView` builds pane A before pane B exists.
	const pair: { a?: EditorView; b?: EditorView } | null = mergeViewSiblings(view);
	if (pair === null) {
		return null;
	}
	const side = pair.a === view ? 'a' : 'b';
	const other = side === 'a' ? pair.b : pair.a;
	if (other === undefined || other.state.field(collapseField, false) === undefined) {
		return null;
	}
	return { other, side };
}

/** Make `next` the collapsed ranges of `view`, and the same lines
 *  those of its sibling pane. A range with no counterpart on the
 *  other side opens on both. */
function commit(view: EditorView, next: readonly LineRange[]): void {
	const sibling = siblingOf(view);
	let own = next;
	if (sibling !== null) {
		const { other, side } = sibling;
		const chunks = getChunks(view.state)?.chunks ?? [];
		const docA = side === 'a' ? view.state.doc : other.state.doc;
		const docB = side === 'a' ? other.state.doc : view.state.doc;
		const runs = unchangedRuns(
			chunks.map((chunk) => changedLinesOf(docA, chunk.fromA, chunk.toA)),
			chunks.map((chunk) => changedLinesOf(docB, chunk.fromB, chunk.toB)),
			docA.lines,
			docB.lines,
		);
		const kept: LineRange[] = [];
		const mapped: LineRange[] = [];
		for (const range of next) {
			const counterpart = mapLineRange(range, runs, side);
			if (counterpart !== null) {
				kept.push(range);
				mapped.push(counterpart);
			}
		}
		own = kept;
		if (!sameRanges(mapped, collapsedLines(other.state))) {
			other.dispatch({ effects: setCollapsed.of(mapped.map((range) => toPositions(other.state.doc, range))) });
		}
	}
	if (!sameRanges(own, collapsedLines(view.state))) {
		view.dispatch({ effects: setCollapsed.of(own.map((range) => toPositions(view.state.doc, range))) });
	}
}

/** Collapse exactly `ranges` in `view` (and in its sibling pane):
 *  how a rebuilt editor gets back what its predecessor had open. */
export function restoreCollapsed(view: EditorView, ranges: readonly LineRange[]): void {
	const lines = view.state.doc.lines;
	commit(
		view,
		ranges.filter((range) => range.from >= 1 && range.to <= lines && range.from <= range.to),
	);
}

function runCommand(view: EditorView, bar: HTMLElement, command: BarCommand): void {
	const state = view.state;
	const pos = view.posAtDOM(bar);
	const value = state.field(collapseField);
	const hit = value.ranges.find((range) => range.from <= pos && pos <= range.to);
	if (hit === undefined) {
		return;
	}
	const range = toLines(state.doc, hit);
	let reveal: LineRange = range;
	if (command !== 'all') {
		// The bar was planned with whatever had been parsed when it was
		// drawn; act on the whole tree so the block's end is known.
		const tree = ensureSyntaxTree(state, state.doc.length, 200) ?? syntaxTree(state);
		const action = planFor(state, range, tree, ownChanges(state))[command];
		if (action === null) {
			return;
		}
		reveal = action.reveal;
	}
	commit(view, revealLines(collapsedLines(state), reveal));
}

/** The visible lines between `bar`'s collapsed run and the next one. */
function blockAfter(view: EditorView, bar: HTMLElement): LineRange | null {
	const state = view.state;
	const pos = view.posAtDOM(bar);
	const ranges = state.field(collapseField).ranges;
	const index = ranges.findIndex((range) => range.from <= pos && pos <= range.to);
	const hit = ranges[index];
	if (hit === undefined) {
		return null;
	}
	const from = toLines(state.doc, hit).to + 1;
	if (from > state.doc.lines) {
		return null;
	}
	const next = ranges[index + 1];
	const to = next === undefined ? state.doc.lines : toLines(state.doc, next).from - 1;
	return { from, to: Math.max(from, to) };
}

class CollapseSync {
	// A pane whose ranges moved on their own (an edit, a rebuild)
	// carries them to its sibling; one that was merely told its
	// ranges does not echo them back.
	private dirty = true;
	private scheduled = false;
	private destroyed = false;

	constructor(private readonly view: EditorView) {
		this.schedule();
	}

	update(update: ViewUpdate): void {
		const before = update.startState.field(collapseField, false);
		const after = update.state.field(collapseField, false);
		const pinsChanged = update.startState.facet(diffCollapsePins) !== update.state.facet(diffCollapsePins);
		if (before?.ranges === after?.ranges && !pinsChanged) {
			return;
		}
		const told = update.transactions.some((tr) => tr.effects.some((effect) => effect.is(setCollapsed)));
		if (!told) {
			this.dirty = true;
		}
		this.schedule();
	}

	destroy(): void {
		this.destroyed = true;
	}

	// Deferred: the sibling only has this update's chunks once
	// `MergeView` has finished dispatching to both panes.
	private schedule(): void {
		if (this.scheduled) {
			return;
		}
		this.scheduled = true;
		queueMicrotask(() => {
			this.scheduled = false;
			if (!this.destroyed) {
				this.sync();
			}
		});
	}

	private sync(): void {
		const state = this.view.state;
		const current = collapsedLines(state);
		let next = current;
		for (const line of state.facet(diffCollapsePins)) {
			next = revealLines(next, { from: line - COLLAPSE_MARGIN, to: line + COLLAPSE_MARGIN });
		}
		const revealed = !sameRanges(next, current);
		if (!revealed && !this.dirty) {
			return;
		}
		this.dirty = false;
		commit(this.view, next);
	}
}

const barGutterMarker = new (class extends GutterMarker {
	override elementClass = 'cm-diffCollapse-gutter';
})();

const collapseTheme = EditorView.baseTheme({
	'.cm-diffCollapse': {
		boxSizing: 'border-box',
		height: '26px',
		borderTop: '1px solid var(--m-border)',
		borderBottom: '1px solid var(--m-border)',
		background: 'var(--m-bg-1)',
		color: 'var(--m-fg-muted)',
		fontFamily: 'var(--m-font-ui)',
		fontSize: 'var(--m-text-xs)',
		cursor: 'default',
		userSelect: 'none',
	},
	// Pinned to the visible pane: the widget itself is as wide as the
	// longest line, which would push the actions out of sight.
	'.cm-diffCollapse-row': {
		position: 'sticky',
		left: 'var(--m-gutter-w, 0px)',
		boxSizing: 'border-box',
		width: 'calc(var(--m-pane-w, 100%) - var(--m-gutter-w, 0px))',
		height: '100%',
		display: 'flex',
		alignItems: 'center',
		gap: '10px',
		padding: '0 8px 0 6px',
		whiteSpace: 'nowrap',
	},
	'.cm-diffCollapse-count': {
		flexShrink: '0',
		fontVariantNumeric: 'tabular-nums',
	},
	'.cm-diffCollapse-symbol': {
		flex: '0 1 auto',
		minWidth: '0',
		overflow: 'hidden',
		textOverflow: 'ellipsis',
		padding: '0',
		border: 'none',
		background: 'transparent',
		color: 'var(--m-fg-muted)',
		fontFamily: 'var(--m-font-mono)',
		fontSize: 'var(--m-text-sm)',
		textAlign: 'left',
	},
	'button.cm-diffCollapse-symbol': {
		cursor: 'pointer',
	},
	'button.cm-diffCollapse-symbol:hover': {
		color: 'var(--m-fg)',
		textDecoration: 'underline',
	},
	'.cm-diffCollapse-actions': {
		flexShrink: '0',
		display: 'flex',
		gap: '2px',
		marginLeft: 'auto',
	},
	'.cm-diffCollapse-btn': {
		padding: '1px 6px',
		border: '1px solid transparent',
		borderRadius: 'var(--m-radius-sm)',
		background: 'transparent',
		color: 'var(--m-fg-muted)',
		font: 'inherit',
		cursor: 'pointer',
	},
	'.cm-diffCollapse-btn:hover': {
		color: 'var(--m-fg)',
		borderColor: 'var(--m-border-strong)',
		background: 'var(--m-wash-hover)',
	},
	'.cm-diffCollapse-ask': {
		color: 'var(--m-accent)',
	},
	'.cm-gutterElement.cm-diffCollapse-gutter': {
		boxSizing: 'border-box',
		borderTop: '1px solid var(--m-border)',
		borderBottom: '1px solid var(--m-border)',
		background: 'var(--m-bg-1)',
	},
});

/**
 * Collapse long unchanged runs behind an expandable bar. Install on
 * each pane of a `MergeView`, or on a `unifiedMergeView` editor.
 */
export function diffCollapse(): Extension {
	return [
		collapseField,
		ViewPlugin.fromClass(CollapseSync),
		gutterWidgetClass.of((_view, widget) => (widget instanceof CollapseBarWidget ? barGutterMarker : null)),
		collapseTheme,
		paneMetrics,
	];
}
