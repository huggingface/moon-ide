// Diff-view helper: tag every line that sits inside a whole-chunk
// addition (only B has content) or whole-chunk deletion (only A has
// content) with the line class `cm-moon-pure-change`. The diff view's
// CSS then suppresses the inner `cm-changedText` per-character tint
// on those lines — the gutter bar plus the line background already
// say "this entire line is new / gone", so the saturated green-on-
// green or red-on-red character highlight reads as noise without
// adding information. Lines inside a *modified* chunk are not
// tagged, so the per-character highlight stays where it actually
// distinguishes the changed substring from unchanged surrounding
// text.
//
// We query `getChunks(state)` from `@codemirror/merge` instead of
// trying to detect the case from the DOM: the merge view wraps the
// whole chunk range in a `<ins>` / `<del>` mark before adding
// `cm-changedText` marks for character-level diffs, and syntax
// highlight spans nest in between, so `:has(> … :only-child)` is
// not reliable. The chunks API gives us the same data the merge
// view itself uses to draw line decorations.

import { getChunks } from '@codemirror/merge';
import { RangeSetBuilder } from '@codemirror/state';
import { Decoration, type DecorationSet, EditorView, ViewPlugin, type ViewUpdate } from '@codemirror/view';

const pureChangeLine = Decoration.line({ class: 'cm-moon-pure-change' });

function compute(view: EditorView): DecorationSet {
	const info = getChunks(view.state);
	if (!info || info.side === null) {
		return Decoration.none;
	}
	const isA = info.side === 'a';
	const builder = new RangeSetBuilder<Decoration>();
	for (const chunk of info.chunks) {
		// Pure add: chunk has no content in A, only in B.
		// Pure delete: chunk has no content in B, only in A.
		// Only tag the side that actually shows the affected lines:
		// pure adds are visible on B, pure deletes on A.
		const isPureAdd = chunk.fromA === chunk.toA;
		const isPureDelete = chunk.fromB === chunk.toB;
		if (isA ? !isPureDelete : !isPureAdd) {
			continue;
		}
		const from = isA ? chunk.fromA : chunk.fromB;
		// `endA` / `endB` is the end of the last line in the chunk
		// (or `fromA` / `fromB` when the chunk is empty on that
		// side, which `isPureAdd` / `isPureDelete` already ruled
		// out above). Walk line-by-line so chunks that span more
		// than one line all get tagged.
		const to = isA ? chunk.endA : chunk.endB;
		const doc = view.state.doc;
		let lineNo = doc.lineAt(from).number;
		while (lineNo <= doc.lines) {
			const line = doc.line(lineNo);
			builder.add(line.from, line.from, pureChangeLine);
			if (line.to >= to) {
				break;
			}
			lineNo++;
		}
	}
	return builder.finish();
}

/**
 * Per-pane extension. Recomputes when the merge view's chunk set
 * changes (which happens whenever either pane's document is edited
 * — both panes get a state update when chunks rebuild, so this
 * `update` runs on both panes in lockstep).
 */
export const diffPureChangeExtension = ViewPlugin.fromClass(
	class {
		decorations: DecorationSet;
		constructor(view: EditorView) {
			this.decorations = compute(view);
		}
		update(u: ViewUpdate) {
			const prev = getChunks(u.startState)?.chunks;
			const next = getChunks(u.state)?.chunks;
			if (prev !== next) {
				this.decorations = compute(u.view);
			}
		}
	},
	{ decorations: (v) => v.decorations },
);
