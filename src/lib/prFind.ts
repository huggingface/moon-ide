// Find across every file of a pull request's diff (the PR tab's
// Ctrl+F). A file's new version is searched whole, its old version
// only where lines were removed, so an unchanged line counts once.
// Hits land on the pane that shows them; one on a removed line also
// names where the removal sits in the new version, which is all a
// unified diff can scroll to.

import { StateEffect, StateField, type Extension } from '@codemirror/state';
import { Decoration, EditorView, type DecorationSet } from '@codemirror/view';

export type FindHit = {
	path: string;
	/** `b` the new version, `a` the old one (a removed line). */
	side: 'a' | 'b';
	/** 1-based line on that side. */
	line: number;
	/** Columns of the match in the line. */
	from: number;
	to: number;
	/** Line of the new version the hit is shown at in a unified diff. */
	anchor: number;
	/** For a removed line, its place in the run of removed lines shown
	 *  together there. */
	index: number;
};

/** Lines of the old version that were removed: each with the line of
 *  the new version the removal sits before, and its place in that
 *  run of removed lines. */
export type RemovedLines = ReadonlyMap<number, { anchor: number; index: number }>;

export type FindFile = { path: string; after: string; base: string; removed: RemovedLines };

function lineHits(text: string, needle: string, caseSensitive: boolean): { from: number; to: number }[] {
	const hay = caseSensitive ? text : text.toLowerCase();
	const hits = [];
	let at = hay.indexOf(needle);
	while (at !== -1) {
		hits.push({ from: at, to: at + needle.length });
		at = hay.indexOf(needle, at + Math.max(needle.length, 1));
	}
	return hits;
}

/** Every hit of `query` in `files`, in reading order: file by file,
 *  and in a file by line of the new version. */
export function findInFiles(files: readonly FindFile[], query: string, caseSensitive = false): FindHit[] {
	if (query === '') {
		return [];
	}
	const needle = caseSensitive ? query : query.toLowerCase();
	const out: FindHit[] = [];
	for (const file of files) {
		const hits: FindHit[] = [];
		file.after.split('\n').forEach((text, index) => {
			for (const hit of lineHits(text, needle, caseSensitive)) {
				hits.push({ path: file.path, side: 'b', line: index + 1, ...hit, anchor: index + 1, index: 0 });
			}
		});
		const baseLines = file.base.split('\n');
		for (const [line, at] of file.removed) {
			for (const hit of lineHits(baseLines[line - 1] ?? '', needle, caseSensitive)) {
				hits.push({ path: file.path, side: 'a', line, ...hit, anchor: at.anchor, index: at.index });
			}
		}
		// Removed lines read before the line they were replaced by.
		hits.sort((x, y) => x.anchor - y.anchor || (x.side === y.side ? x.line - y.line : x.side === 'a' ? -1 : 1));
		out.push(...hits);
	}
	return out;
}

/** The removed lines of `base` → `after`, from a line diff's changed
 *  ranges (`fromA`/`toA` in the old text, `fromB` in the new, as
 *  character offsets). */
export function removedLines(
	base: string,
	after: string,
	chunks: readonly { fromA: number; toA: number; fromB: number }[],
): RemovedLines {
	const lineAt = (text: string, offset: number) => text.slice(0, offset).split('\n').length;
	const removed = new Map<number, { anchor: number; index: number }>();
	for (const chunk of chunks) {
		if (chunk.toA <= chunk.fromA) {
			continue;
		}
		const first = lineAt(base, chunk.fromA);
		const last = lineAt(base, Math.max(chunk.fromA, chunk.toA - 1));
		const anchor = lineAt(after, chunk.fromB);
		for (let line = first; line <= last; line++) {
			removed.set(line, { anchor, index: line - first });
		}
	}
	return removed;
}

/** Marks for the hits shown in one pane: every hit, and the current
 *  one apart. `null` clears them. */
export const setFindMarks = StateEffect.define<{ ranges: { from: number; to: number }[]; current: number } | null>();

const findMarksField = StateField.define<DecorationSet>({
	create: () => Decoration.none,
	update(marks, tr) {
		let next = marks.map(tr.changes);
		for (const effect of tr.effects) {
			if (!effect.is(setFindMarks)) {
				continue;
			}
			if (effect.value === null) {
				next = Decoration.none;
				continue;
			}
			const { ranges, current } = effect.value;
			next = Decoration.set(
				ranges
					.filter((range) => range.to > range.from && range.to <= tr.state.doc.length)
					.map((range, index) =>
						Decoration.mark({
							class: index === current ? 'cm-searchMatch cm-searchMatch-selected' : 'cm-searchMatch',
						}).range(range.from, range.to),
					),
				true,
			);
		}
		return next;
	},
	provide: (field) => EditorView.decorations.from(field),
});

/** The pane extension that draws the hits, as the editor's own search
 *  draws its matches. */
export function findMarks(): Extension {
	return findMarksField;
}

const REMOVED_HIGHLIGHT = 'pr-find-removed';

/** Show the current hit on a removed line of a unified diff, which is
 *  drawn outside the editor's text (so no mark can reach it), and
 *  return the line's element to scroll to. `null` hit clears it. */
export function markRemovedHit(view: EditorView, hit: FindHit | null): HTMLElement | null {
	const highlights = typeof CSS === 'undefined' ? undefined : CSS.highlights;
	highlights?.delete(REMOVED_HIGHLIGHT);
	if (hit === null) {
		return null;
	}
	const chunk = [...view.dom.querySelectorAll<HTMLElement>('.cm-deletedChunk')].find((el) => {
		const pos = view.posAtDOM(el);
		return view.state.doc.lineAt(pos).number === hit.anchor;
	});
	const lineEl = chunk?.querySelectorAll<HTMLElement>('.cm-deletedLine')[hit.index] ?? null;
	if (lineEl === null || highlights === undefined) {
		return lineEl;
	}
	const range = document.createRange();
	const walker = document.createTreeWalker(lineEl, NodeFilter.SHOW_TEXT);
	let seen = 0;
	let started = false;
	for (let node = walker.nextNode(); node !== null; node = walker.nextNode()) {
		const length = node.textContent?.length ?? 0;
		if (!started && hit.from < seen + length) {
			range.setStart(node, hit.from - seen);
			started = true;
		}
		if (started && hit.to <= seen + length) {
			range.setEnd(node, hit.to - seen);
			highlights.set(REMOVED_HIGHLIGHT, new Highlight(range));
			break;
		}
		seen += length;
	}
	return lineEl;
}
