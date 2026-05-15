// Diff-view helper: tag every line whose entire content is part of
// a server-reported text change with the line class
// `cm-moon-pure-change`. The diff view's CSS then suppresses the
// inner `cm-changedText` per-character tint on those lines — the
// gutter cell tint plus the line background already say "this
// entire line is new / gone", so the saturated green-on-green or
// red-on-red character highlight reads as noise without adding
// information.
//
// Two cases that earn the tag:
//
// 1. **Whole-chunk pure add / pure delete.** A chunk that's empty
//    on one side (`fromA === toA` or `fromB === toB`). Every line
//    on the populated side qualifies — no characters survived from
//    the other side by construction.
// 2. **Pure-new / pure-deleted lines inside a modified chunk.**
//    The common case: a refactor that adds 5 comment lines between
//    two unchanged lines registers as one *modified* chunk because
//    the surrounding indent / punctuation aligns as common
//    substrings. Old behaviour kept the per-character highlight
//    across the whole chunk, painting the 5 new lines in
//    saturated green from edge to edge. New behaviour walks each
//    line and checks whether its entire range is covered by
//    `Change.fromB..toB` (B side) / `fromA..toA` (A side) spans;
//    a line with no surviving characters from the other side
//    earns the tag, while a line that has some common substring
//    (typically the boundary lines of the chunk — indent, trailing
//    comma, etc.) keeps the highlight so the surviving fragments
//    stay visible.
//
// Lines that genuinely share substrings with the opposite side (a
// `foo()` → `bar()` rename, a punctuation tweak inside an
// otherwise-unchanged statement) are *not* tagged, so the
// per-character highlight stays where it actually distinguishes
// the changed substring from unchanged surrounding text.
//
// We query `getChunks(state)` from `@codemirror/merge` instead of
// trying to detect the case from the DOM: the merge view wraps the
// whole chunk range in a `<ins>` / `<del>` mark before adding
// `cm-changedText` marks for character-level diffs, and syntax
// highlight spans nest in between, so `:has(> … :only-child)` is
// not reliable. The chunks API gives us the same data the merge
// view itself uses to draw line decorations.

import type { Change } from '@codemirror/merge';
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
	const doc = view.state.doc;
	for (const chunk of info.chunks) {
		const chunkFrom = isA ? chunk.fromA : chunk.fromB;
		const chunkContentTo = isA ? chunk.toA : chunk.toB;
		// Chunk empty on this side (pure-add seen from A or
		// pure-delete seen from B) — nothing to tag here, the
		// other pane's iteration covers it.
		if (chunkFrom === chunkContentTo) {
			continue;
		}
		// Walk line-by-line through the chunk's range on this
		// side. `endA` / `endB` is the end of the last line in
		// the chunk; we step from `chunkFrom` to there inclusive.
		const chunkEnd = isA ? chunk.endA : chunk.endB;
		let lineNo = doc.lineAt(chunkFrom).number;
		while (lineNo <= doc.lines) {
			const line = doc.line(lineNo);
			if (lineFullyCovered(line, chunk.changes, chunkFrom, isA)) {
				builder.add(line.from, line.from, pureChangeLine);
			}
			if (line.to >= chunkEnd) {
				break;
			}
			lineNo++;
		}
	}
	return builder.finish();
}

/**
 * `true` iff every character of `line` (excluding the trailing
 * newline, which CM doesn't include in `[line.from, line.to]`) is
 * inside some `Change.fromA..toA` / `fromB..toB` span. An empty
 * line counts as covered: it has no content that could survive
 * from the other side.
 *
 * Changes inside a chunk are guaranteed sorted by `fromA` /
 * `fromB` (the merge package builds them sequentially), so a
 * single forward pass with a cursor catches every common gap.
 *
 * Exported for unit testing; not part of the runtime API.
 */
export function lineFullyCovered(
	line: { from: number; to: number },
	changes: readonly Change[],
	chunkFrom: number,
	isA: boolean,
): boolean {
	if (line.from === line.to) {
		return true;
	}
	// Map the line range into chunk-relative offsets, matching
	// the coordinate space `Change.fromA` / `fromB` live in.
	const lineFrom = line.from - chunkFrom;
	const lineTo = line.to - chunkFrom;
	let cursor = lineFrom;
	for (const ch of changes) {
		const cFrom = isA ? ch.fromA : ch.fromB;
		const cTo = isA ? ch.toA : ch.toB;
		if (cTo <= cursor) {
			continue;
		}
		if (cFrom > cursor) {
			// Gap of common content (substring shared with the
			// other side) sitting on this line — the line keeps
			// the per-character highlight so the surviving
			// fragment is still visible.
			return false;
		}
		cursor = cTo;
		if (cursor >= lineTo) {
			return true;
		}
	}
	return cursor >= lineTo;
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
