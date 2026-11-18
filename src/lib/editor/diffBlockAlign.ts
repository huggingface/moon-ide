// Keeps the two panes of a split diff level around cards.
//
// A card (draft comment, composer, pull-request thread or note) is a
// block widget in one pane only. `@codemirror/merge` re-aligns the
// panes at the start of each unchanged run and nowhere else, so
// every line between a card and the next change sits lower than its
// counterpart by the card's height. This puts a filler of the same
// height at the corresponding line of the other pane.
//
// Only cards under an *unchanged* line get a filler. Inside a change
// the lines have no counterpart to stay level with, and the package's
// own spacer already absorbs the difference where the change ends.
//
// Fillers are measured, so they exist for cards that are currently
// drawn; a card scrolled far out of view keeps the filler it last
// had. Card heights do not depend on fillers or on the package's
// spacers, so the two passes settle after one round each.

import { getChunks, mergeViewSiblings, type Chunk } from '@codemirror/merge';
import { StateEffect, StateField, type Extension, type Range } from '@codemirror/state';
import { Decoration, EditorView, ViewPlugin, WidgetType, type DecorationSet, type ViewUpdate } from '@codemirror/view';
import { BLOCK_CLASS } from './blockCards';
import { changedLinesOf, mapLineRange, unchangedRuns, type UnchangedRun } from './diffCollapse';

class Filler extends WidgetType {
	constructor(readonly height: number) {
		super();
	}

	override eq(other: Filler): boolean {
		return Math.abs(other.height - this.height) < 0.5;
	}

	override toDOM(): HTMLElement {
		const el = document.createElement('div');
		el.className = 'cm-mergeSpacer cm-moon-filler';
		el.style.height = `${this.height}px`;
		return el;
	}

	override updateDOM(dom: HTMLElement): boolean {
		dom.style.height = `${this.height}px`;
		return true;
	}

	override get estimatedHeight(): number {
		return this.height;
	}

	override ignoreEvent(): boolean {
		return false;
	}
}

const setFillers = StateEffect.define<DecorationSet>({
	map: (value, mapping) => value.map(mapping),
});

const fillerField = StateField.define<DecorationSet>({
	create: () => Decoration.none,
	update(value, tr) {
		for (const effect of tr.effects) {
			if (effect.is(setFillers)) {
				return effect.value;
			}
		}
		return tr.docChanged ? value.map(tr.changes) : value;
	},
	provide: (field) => EditorView.decorations.from(field),
});

type Side = 'a' | 'b';
type Pair = Record<Side, EditorView>;

/** Both panes, once both exist: `MergeView` builds A before B. */
function siblings(view: EditorView): Pair | null {
	const pair: Partial<Pair> | null = mergeViewSiblings(view);
	if (pair === null || pair.a === undefined || pair.b === undefined) {
		return null;
	}
	return { a: pair.a, b: pair.b };
}

/** Card height under each line of `view`, for the cards drawn now. */
function measureCards(view: EditorView): Map<number, number> {
	const heights = new Map<number, number>();
	for (const el of view.contentDOM.querySelectorAll<HTMLElement>(`.${BLOCK_CLASS}`)) {
		const line = view.state.doc.lineAt(view.posAtDOM(el)).number;
		heights.set(line, (heights.get(line) ?? 0) + el.getBoundingClientRect().height);
	}
	return heights;
}

function drawnLines(view: EditorView): { from: number; to: number } {
	const doc = view.state.doc;
	return { from: doc.lineAt(view.viewport.from).number, to: doc.lineAt(view.viewport.to).number };
}

function counterpart(line: number, runs: readonly UnchangedRun[], from: Side): number | null {
	return mapLineRange({ from: line, to: line }, runs, from)?.from ?? null;
}

function sameFillers(a: DecorationSet, b: DecorationSet): boolean {
	if (a.size !== b.size) {
		return false;
	}
	const iterA = a.iter();
	const iterB = b.iter();
	while (iterA.value !== null && iterB.value !== null) {
		if (iterA.from !== iterB.from || !iterA.value.spec.widget.eq(iterB.value.spec.widget)) {
			return false;
		}
		iterA.next();
		iterB.next();
	}
	return true;
}

function align(pair: Pair): void {
	const chunks = getChunks(pair.b.state)?.chunks;
	if (chunks === undefined) {
		return;
	}
	const docs = { a: pair.a.state.doc, b: pair.b.state.doc };
	const runs = unchangedRuns(
		chunks.map((chunk) => changedLinesOf(docs.a, chunk.fromA, chunk.toA)),
		chunks.map((chunk) => changedLinesOf(docs.b, chunk.fromB, chunk.toB)),
		docs.a.lines,
		docs.b.lines,
	);
	const drawn = { a: drawnLines(pair.a), b: drawnLines(pair.b) };
	// A line pair is ours to decide when both of its lines are drawn:
	// only then are the cards on both sides of it measurable.
	const decidable = (lineA: number, lineB: number) =>
		lineA >= drawn.a.from && lineA <= drawn.a.to && lineB >= drawn.b.from && lineB <= drawn.b.to;

	// Keyed by the pair's line in A.
	const cards = new Map<number, { lineB: number; a: number; b: number }>();
	for (const side of ['a', 'b'] as const) {
		for (const [line, height] of measureCards(pair[side])) {
			const other = counterpart(line, runs, side);
			if (other === null) {
				continue;
			}
			const lineA = side === 'a' ? line : other;
			const lineB = side === 'a' ? other : line;
			if (!decidable(lineA, lineB)) {
				continue;
			}
			const entry = cards.get(lineA) ?? { lineB, a: 0, b: 0 };
			entry[side] += height;
			cards.set(lineA, entry);
		}
	}

	const tail = tailGap(pair, chunks);

	for (const side of ['a', 'b'] as const) {
		const view = pair[side];
		const doc = docs[side];
		const other: Side = side === 'a' ? 'b' : 'a';
		const current = view.state.field(fillerField);
		const next: Range<Decoration>[] = [];
		// Fillers we cannot measure for right now stay as they are.
		for (const iter = current.iter(); iter.value !== null; iter.next()) {
			if (iter.value.spec.side === TAIL_SIDE) {
				if (tail === null) {
					next.push(iter.value.range(iter.from));
				}
				continue;
			}
			const line = doc.lineAt(iter.from).number;
			const twin = counterpart(line, runs, side);
			const stale = twin === null || decidable(side === 'a' ? line : twin, side === 'a' ? twin : line);
			if (!stale) {
				next.push(iter.value.range(iter.from));
			}
		}
		for (const [lineA, entry] of cards) {
			const missing = entry[other] - entry[side];
			if (missing < 0.5) {
				continue;
			}
			const line = side === 'a' ? lineA : entry.lineB;
			// After the pane's own cards on that line (theirs are side 1-2).
			next.push(Decoration.widget({ widget: new Filler(missing), block: true, side: 3 }).range(doc.line(line).to));
		}
		const missingTail = tail === null ? 0 : side === 'a' ? tail : -tail;
		if (missingTail >= 0.5) {
			next.push(Decoration.widget({ widget: new Filler(missingTail), block: true, side: TAIL_SIDE }).range(doc.length));
		}
		const set = Decoration.set(next, true);
		if (!sameFillers(set, current)) {
			view.dispatch({ effects: setFillers.of(set) });
		}
	}
}

const TAIL_SIDE = -1;

/**
 * How much lower the final empty line sits in B than in A when the
 * last change runs right up to it (a file ending in a newline, with
 * lines appended or removed at its end). The package aligns a run at
 * its first character, and that last line has none, so it is left
 * wherever the change before it pushed it. `null` when the end of
 * either document is not drawn, or there is nothing to fix.
 *
 * Read off each pane's height map at the top of the line's block,
 * which includes a filler already placed above the line, so the
 * answer does not move once the filler is in.
 */
function tailGap(pair: Pair, chunks: readonly Chunk[]): number | null {
	const last = chunks[chunks.length - 1];
	const endA = pair.a.state.doc.length;
	const endB = pair.b.state.doc.length;
	if (last === undefined || endA === 0 || endB === 0 || last.toA !== endA || last.toB !== endB) {
		return null;
	}
	if (pair.a.viewport.to < endA || pair.b.viewport.to < endB) {
		return null;
	}
	return pair.b.lineBlockAt(endB).top - pair.a.lineBlockAt(endA).top;
}

// One pending pass per merge view, whichever pane asked for it.
const pending = new WeakMap<EditorView, number>();

class AlignPlugin {
	private readonly observer: ResizeObserver;
	private observed = new Set<Element>();

	constructor(private readonly view: EditorView) {
		// A card can change height with no editor update at all: its
		// markdown arriving, a resolved thread unfolding.
		this.observer = new ResizeObserver(() => {
			this.view.requestMeasure();
			this.schedule();
		});
		this.schedule();
	}

	update(update: ViewUpdate): void {
		const reconfigured = update.transactions.some((tr) => tr.reconfigured);
		const chunksChanged = getChunks(update.startState)?.chunks !== getChunks(update.state)?.chunks;
		if (
			update.docChanged ||
			update.viewportChanged ||
			update.geometryChanged ||
			update.heightChanged ||
			reconfigured ||
			chunksChanged
		) {
			this.schedule();
		}
	}

	destroy(): void {
		this.observer.disconnect();
	}

	private schedule(): void {
		const pair = siblings(this.view);
		if (pair === null || pending.has(pair.a)) {
			return;
		}
		pending.set(
			pair.a,
			requestAnimationFrame(() => {
				pending.delete(pair.a);
				if (!pair.a.dom.isConnected || !pair.b.dom.isConnected) {
					return;
				}
				const pluginA = pair.a.plugin(alignPlugin);
				const pluginB = pair.b.plugin(alignPlugin);
				if (pluginA === null || pluginB === null) {
					return;
				}
				pluginA.observeCards();
				pluginB.observeCards();
				align(pair);
			}),
		);
	}

	observeCards(): void {
		const seen = new Set<Element>(this.view.contentDOM.querySelectorAll(`.${BLOCK_CLASS}`));
		for (const el of this.observed) {
			if (!seen.has(el)) {
				this.observer.unobserve(el);
			}
		}
		for (const el of seen) {
			if (!this.observed.has(el)) {
				this.observer.observe(el);
			}
		}
		this.observed = seen;
	}
}

const alignPlugin = ViewPlugin.fromClass(AlignPlugin);

/**
 * Mirror the height of each pane's cards into the other pane.
 * Install on both panes of a `MergeView`; a no-op anywhere else.
 */
export const diffBlockAlign: Extension = [fillerField, alignPlugin];
