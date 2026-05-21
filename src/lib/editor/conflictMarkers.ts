// CodeMirror decoration for in-buffer merge conflict markers.
//
// What it does:
//
//   - Spans a tinted background across every line of an unresolved
//     conflict block (`<<<<<<<` → `>>>>>>>`, with optional
//     `|||||||` base section in `diff3` style).
//   - Renders an inline widget toolbar on the opening `<<<<<<<`
//     line: `Accept current` / `Accept incoming` / `Accept both`.
//     Each button rewrites the block in place by deleting the
//     marker lines and the half the user rejected; the file then
//     looks like the user manually picked a side and the row's
//     conflict badge clears the moment they save (the
//     `auto-stage-on-save` hook in `state.svelte.ts` runs `git add`
//     against any file whose unmerged status falls off after the
//     write).
//
// What it deliberately doesn't do:
//
//   - Scan files we don't already know are conflicted. The
//     `headTextFacet` pattern wouldn't help here because conflict
//     markers can live in clean files (this very source file
//     references them in comments); we gate the decorator on the
//     active folder's `gitStatusEntries` reporting `conflicted`
//     for the buffer's path via the `conflictedFacet` below.
//   - Try to be cleverer than line-prefix matching. Real `git`
//     emits these markers at column 0 and counts the seven `<`
//     literally; we do the same so a stray `>>>>>>>` deep in a
//     code comment doesn't trip the widget.
//
// The toolbar uses Pierre-style flat buttons rather than CSS
// pseudo-elements so future "Edit manually" / "Reject all" actions
// can drop in without rewriting the renderer.

import { Facet, RangeSetBuilder, StateField, type EditorState, type Extension, type Range } from '@codemirror/state';
import { Decoration, EditorView, WidgetType, type DecorationSet } from '@codemirror/view';

/**
 * `true` iff the active editor's file is in the workspace's
 * `gitStatusEntries` with `status === 'conflicted'`. The Editor
 * wires this from `workspace.gitStatusEntries`; the decoration
 * StateField short-circuits to an empty set when `false` so the
 * extension is harmless on every other buffer (including
 * documentation files that happen to contain the marker syntax).
 */
export const conflictedFacet = Facet.define<boolean, boolean>({
	combine: (values) => values.some((v) => v),
});

type ConflictBlock = {
	/** 1-based line number of the `<<<<<<<` marker. */
	startLine: number;
	/** 1-based line number of the `=======` separator. */
	separatorLine: number;
	/** 1-based line number of the `>>>>>>>` marker. */
	endLine: number;
	/**
	 * 1-based line number of the optional `|||||||` base
	 * separator (diff3 conflict style). `null` for the regular
	 * two-way conflict — most files. The accept widgets ignore
	 * the base section either way: "Accept current" / "Accept
	 * incoming" both produce a single clean side, never the
	 * base.
	 */
	baseLine: number | null;
};

/**
 * Scan `state.doc` for top-level conflict markers. We only
 * recognise the canonical column-0 form `git merge` emits — any
 * indented occurrence (e.g. inside a string literal) is ignored.
 * Mismatched blocks (a `<<<<<<<` without a matching `=======` and
 * `>>>>>>>`) are skipped silently; the user can finish typing.
 */
function findConflictBlocks(state: EditorState): ConflictBlock[] {
	const out: ConflictBlock[] = [];
	const doc = state.doc;
	let i = 1;
	while (i <= doc.lines) {
		const line = doc.line(i);
		if (line.text.startsWith('<<<<<<<')) {
			let separator: number | null = null;
			let base: number | null = null;
			let end: number | null = null;
			for (let j = i + 1; j <= doc.lines; j++) {
				const inner = doc.line(j).text;
				if (inner.startsWith('<<<<<<<')) {
					// Nested / malformed — bail on this block and
					// resume scanning from the inner marker so we
					// don't lose later well-formed blocks.
					i = j;
					separator = null;
					end = null;
					break;
				}
				if (separator === null && inner.startsWith('|||||||')) {
					base = j;
					continue;
				}
				if (separator === null && inner.startsWith('=======')) {
					separator = j;
					continue;
				}
				if (separator !== null && inner.startsWith('>>>>>>>')) {
					end = j;
					break;
				}
			}
			if (separator !== null && end !== null) {
				out.push({ startLine: i, separatorLine: separator, endLine: end, baseLine: base });
				i = end + 1;
				continue;
			}
		}
		i += 1;
	}
	return out;
}

/** Tinted background for every line in the block. */
const conflictBlockDecoration = Decoration.line({ class: 'cm-conflict-block' });
/** Stronger tint on the `<<<<<<<` / `=======` / `>>>>>>>` lines themselves. */
const conflictMarkerLineDecoration = Decoration.line({ class: 'cm-conflict-marker-line' });

class AcceptToolbarWidget extends WidgetType {
	constructor(private readonly block: ConflictBlock) {
		super();
	}

	override eq(other: WidgetType): boolean {
		return (
			other instanceof AcceptToolbarWidget &&
			other.block.startLine === this.block.startLine &&
			other.block.separatorLine === this.block.separatorLine &&
			other.block.endLine === this.block.endLine &&
			other.block.baseLine === this.block.baseLine
		);
	}

	toDOM(view: EditorView): HTMLElement {
		const root = document.createElement('span');
		root.className = 'cm-conflict-toolbar';

		const make = (label: string, title: string, handler: () => void) => {
			const btn = document.createElement('button');
			btn.type = 'button';
			btn.className = 'cm-conflict-btn';
			btn.textContent = label;
			btn.title = title;
			// Pierre rows + CodeMirror's own selection layer would
			// otherwise treat the click as a row-selection gesture;
			// stop the propagation so the dispatch lands cleanly.
			btn.addEventListener('mousedown', (e) => e.preventDefault());
			btn.addEventListener('click', (e) => {
				e.stopPropagation();
				handler();
				// Restore focus so the user can immediately hit
				// Ctrl+S — they were inside the editor before the
				// click bumped them out.
				view.focus();
			});
			return btn;
		};

		const acceptSide = (which: 'current' | 'incoming' | 'both') => {
			const tr = buildAcceptTransaction(view.state, this.block, which);
			if (tr !== null) {
				view.dispatch(tr);
			}
		};

		root.appendChild(make('Accept current', 'Keep the version above =======', () => acceptSide('current')));
		root.appendChild(make('Accept incoming', 'Keep the version below =======', () => acceptSide('incoming')));
		root.appendChild(make('Accept both', 'Keep both sides; remove the markers', () => acceptSide('both')));
		return root;
	}

	override ignoreEvent(): boolean {
		// Let CodeMirror keep its keyboard handling outside of the
		// buttons themselves — the buttons handle their own clicks.
		return false;
	}
}

/**
 * Produce a transaction that resolves `block` by accepting one of
 * three sides:
 *
 *   - `current`  → keep lines between `<<<<<<<` (exclusive) and the
 *     base / separator (exclusive); drop everything else.
 *   - `incoming` → keep lines between `=======` (exclusive) and
 *     `>>>>>>>` (exclusive); drop everything else.
 *   - `both`     → keep both sides concatenated; drop only the
 *     marker lines themselves (and the base section if present).
 *
 * Returns `null` if the block's line ranges no longer make sense
 * against the current doc (the user edited the file between the
 * decoration recompute and the click).
 */
function buildAcceptTransaction(
	state: EditorState,
	block: ConflictBlock,
	which: 'current' | 'incoming' | 'both',
): { changes: { from: number; to: number; insert: string } } | null {
	const doc = state.doc;
	if (block.endLine > doc.lines || block.separatorLine >= block.endLine) {
		return null;
	}
	const startLine = doc.line(block.startLine);
	const endLine = doc.line(block.endLine);
	// Replace the whole block, marker lines included. `endLine.to`
	// excludes the trailing newline; we capture it explicitly when
	// the file isn't a single-line buffer so the post-edit doc
	// keeps its line break structure.
	const from = startLine.from;
	const trailingNewline = endLine.to < doc.length ? '\n' : '';
	const to = trailingNewline === '\n' ? endLine.to + 1 : endLine.to;

	const currentTopExclusive = block.startLine + 1;
	const currentBottomExclusive = (block.baseLine ?? block.separatorLine) - 1;
	const incomingTopExclusive = block.separatorLine + 1;
	const incomingBottomExclusive = block.endLine - 1;

	const collect = (top: number, bottom: number): string => {
		if (top > bottom) {
			return '';
		}
		const fromOff = doc.line(top).from;
		const toOff = doc.line(bottom).to;
		return doc.sliceString(fromOff, toOff);
	};

	let insert: string;
	if (which === 'current') {
		insert = collect(currentTopExclusive, currentBottomExclusive);
	} else if (which === 'incoming') {
		insert = collect(incomingTopExclusive, incomingBottomExclusive);
	} else {
		const current = collect(currentTopExclusive, currentBottomExclusive);
		const incoming = collect(incomingTopExclusive, incomingBottomExclusive);
		if (current.length === 0) {
			insert = incoming;
		} else if (incoming.length === 0) {
			insert = current;
		} else {
			insert = `${current}\n${incoming}`;
		}
	}
	if (insert.length > 0 && !insert.endsWith('\n') && trailingNewline === '\n') {
		insert = `${insert}\n`;
	}
	if (insert.length === 0 && trailingNewline === '\n') {
		// Don't leave a dangling empty line where the block sat.
		return { changes: { from, to, insert: '' } };
	}
	return { changes: { from, to, insert } };
}

function buildDecorations(state: EditorState): DecorationSet {
	if (!state.facet(conflictedFacet)) {
		return Decoration.none;
	}
	const blocks = findConflictBlocks(state);
	if (blocks.length === 0) {
		return Decoration.none;
	}
	const ranges: Range<Decoration>[] = [];
	for (const block of blocks) {
		for (let n = block.startLine; n <= block.endLine; n++) {
			const line = state.doc.line(n);
			const isMarker =
				n === block.startLine ||
				n === block.separatorLine ||
				n === block.endLine ||
				(block.baseLine !== null && n === block.baseLine);
			ranges.push((isMarker ? conflictMarkerLineDecoration : conflictBlockDecoration).range(line.from));
		}
		const startLine = state.doc.line(block.startLine);
		ranges.push(
			Decoration.widget({
				widget: new AcceptToolbarWidget(block),
				side: 1,
			}).range(startLine.to),
		);
	}
	// `RangeSetBuilder.add` requires sorted-by-`from` input. The
	// per-block loop above already walks lines in order and the
	// widget anchors on `startLine.to` (between two adjacent
	// `line.from` decorations of subsequent blocks, by
	// construction). Sort defensively in case a future change to
	// the loop breaks the invariant.
	ranges.sort((a, b) => a.from - b.from);
	const builder = new RangeSetBuilder<Decoration>();
	for (const r of ranges) {
		builder.add(r.from, r.to, r.value);
	}
	return builder.finish();
}

const conflictMarkersField = StateField.define<DecorationSet>({
	create: (state) => buildDecorations(state),
	update: (value, tr) => {
		const prev = tr.startState.facet(conflictedFacet);
		const next = tr.state.facet(conflictedFacet);
		if (!tr.docChanged && prev === next) {
			return value;
		}
		return buildDecorations(tr.state);
	},
	provide: (f) => EditorView.decorations.from(f),
});

/**
 * Returns `true` iff the document still contains at least one
 * canonical conflict marker line (column-0 `<<<<<<<` / `=======` /
 * `>>>>>>>`). Used by the commit-merge soft-warn so the user gets
 * a confirm when they're about to commit a file that still has
 * marker text — `git ls-files --unmerged` having emptied the
 * staged index alone doesn't guarantee the worktree is clean.
 */
export function docContainsConflictMarkers(state: EditorState): boolean {
	const doc = state.doc;
	for (let i = 1; i <= doc.lines; i++) {
		const text = doc.line(i).text;
		if (text.startsWith('<<<<<<<') || text.startsWith('=======') || text.startsWith('>>>>>>>')) {
			return true;
		}
	}
	return false;
}

/**
 * The CM extension. Wire as `conflictMarkersExtension()` in
 * `Editor.svelte`'s `baseExtensions()` alongside the
 * `gitChangesExtension` block.
 */
export function conflictMarkersExtension(): Extension {
	return [conflictMarkersField];
}
