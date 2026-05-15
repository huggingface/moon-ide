// Diff-view counterpart to `editor/gitChanges.ts`'s line-number
// background tinting. Queries the `@codemirror/merge` view's
// `getChunks(state)` for the active pane, classifies every line in
// every chunk as pure-add / pure-delete / modified, and feeds the
// result into CodeMirror's `gutterLineClass` facet. The class lands
// on the line-number cell (and any other gutter rows on the same
// line) — CSS in `editor/theme.ts` then paints the cell background
// with the same green / blue / red vocabulary the regular editor's
// git-change indicator uses.
//
// Why this exists alongside the merge package's own `cm-changeGutter`:
//
//   - The package's 3px gutter bar is a dedicated column to the
//     left of line numbers. We replace it with a same-coloured
//     line-number cell background so the chrome stays narrower and
//     the two diff surfaces (editor with git-changes, MergeView)
//     share visual vocabulary. The dedicated column is disabled by
//     passing `gutter: false` to `MergeView`.
//   - The package's `cm-changedLine` line decoration (a soft
//     background tint on the *content* row) stays on — it gives
//     the row-wide cue GitHub also shows. Our additions live on
//     the gutter row, which is a different DOM layer that CSS
//     can't reach from `.cm-changedLine` alone.
//
// Per-pane: pane A surfaces deletions + modifications; pane B
// surfaces additions + modifications. The caller picks which pane
// it's wiring up.

import { getChunks, type Chunk } from '@codemirror/merge';
import { RangeSet, StateField, type EditorState, type Extension } from '@codemirror/state';
import { gutterLineClass, GutterMarker } from '@codemirror/view';

class DiffLineClassMarker extends GutterMarker {
	override readonly elementClass: string;

	constructor(className: string) {
		super();
		this.elementClass = className;
	}

	override eq(other: GutterMarker): boolean {
		return other instanceof DiffLineClassMarker && other.elementClass === this.elementClass;
	}
}

// Cache markers so RangeSet's `eq` can dedupe redraws across
// recomputes — a fresh instance every update would thrash CM's
// gutter diff.
const ADDED = new DiffLineClassMarker('cm-gitline cm-gitline-added');
const MODIFIED = new DiffLineClassMarker('cm-gitline cm-gitline-modified');
const DELETED = new DiffLineClassMarker('cm-gitline cm-gitline-deleted');

/**
 * Build the per-pane gutter-line-class RangeSet from the merge
 * view's current chunks. `wantSide` picks which pane we're
 * decorating; the caller passes whichever facet of the MergeView
 * this extension was installed on.
 */
function buildGutterClassSet(state: EditorState, wantSide: 'a' | 'b'): RangeSet<GutterMarker> {
	const info = getChunks(state);
	if (!info || info.side === null) {
		return RangeSet.empty;
	}
	if (info.side !== wantSide) {
		// `getChunks` reports the side this state belongs to. When
		// it doesn't match (e.g. the user wired pane A's extension
		// into pane B's state by accident), produce nothing rather
		// than tint the wrong column.
		return RangeSet.empty;
	}
	const isA = wantSide === 'a';
	const doc = state.doc;
	const ranges = [];
	for (const chunk of info.chunks) {
		const marker = classifyChunk(chunk, isA);
		if (marker === null) {
			continue;
		}
		const from = isA ? chunk.fromA : chunk.fromB;
		const to = isA ? chunk.endA : chunk.endB;
		// Walk line-by-line: a chunk that spans 5 lines needs 5
		// markers (one per line). `doc.lineAt(from).number` is
		// 1-based; we step until we've covered the chunk's range
		// or run off the end of the doc.
		let lineNo = doc.lineAt(from).number;
		while (lineNo <= doc.lines) {
			const line = doc.line(lineNo);
			ranges.push(marker.range(line.from));
			if (line.to >= to) {
				break;
			}
			lineNo++;
		}
	}
	if (ranges.length === 0) {
		return RangeSet.empty;
	}
	return RangeSet.of(ranges, true);
}

/**
 * Decide which class a chunk earns *on the requested side*:
 *
 *   - Pure add — empty on A, content on B. Visible on B only;
 *     A doesn't get a marker (the chunk has no lines there).
 *   - Pure delete — content on A, empty on B. Visible on A only.
 *   - Modified — content on both sides. Both panes get the
 *     `modified` marker.
 */
function classifyChunk(chunk: Chunk, isA: boolean): DiffLineClassMarker | null {
	const isPureAdd = chunk.fromA === chunk.toA;
	const isPureDelete = chunk.fromB === chunk.toB;
	if (isPureAdd) {
		return isA ? null : ADDED;
	}
	if (isPureDelete) {
		return isA ? DELETED : null;
	}
	return MODIFIED;
}

/**
 * Per-pane extension. `side` declares which `MergeView` pane this
 * extension is installed on; it must match the side the state's
 * `mergeConfig` facet reports via `getChunks`.
 *
 * Implementation goes through a `StateField` rather than
 * `gutterLineClass.compute(['doc'], …)` because the chunks
 * rebuild whenever **either** pane's doc changes — a `['doc']`
 * dependency only re-runs on edits to *this* pane's state, and
 * would leave the gutter stale after an edit on the sibling pane.
 * Diffing the `chunks` reference (`@codemirror/merge` produces a
 * fresh array on every rebuild) catches both directions in one
 * place.
 */
export function diffGutterTintExtension(side: 'a' | 'b'): Extension {
	const field = StateField.define<RangeSet<GutterMarker>>({
		create: (state) => buildGutterClassSet(state, side),
		update: (value, tr) => {
			const prevChunks = getChunks(tr.startState)?.chunks ?? null;
			const nextChunks = getChunks(tr.state)?.chunks ?? null;
			if (prevChunks === nextChunks) {
				return value;
			}
			return buildGutterClassSet(tr.state, side);
		},
		provide: (f) => gutterLineClass.from(f),
	});
	return field;
}
