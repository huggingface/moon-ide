// Per-line git-change markers in the editor gutter.
//
// Pulls the file's `HEAD` content via the workspace cache (populated
// by `WorkspaceState.refreshHead` when a buffer is opened) and, on
// every transaction, line-diffs it against the current doc. The
// resulting per-line classification is surfaced as coloured bars in
// a dedicated gutter:
//
//   - green `added`      — line exists in the working tree but not
//                          in `HEAD` and isn't adjacent to a removal
//   - blue  `modified`   — added block that directly replaces a
//                          removed block; the typical "I changed
//                          this line" case
//   - red   `deletedAbove` / `deletedBelow` — tiny wedges at the
//                          boundary where a pure deletion occurred,
//                          so a removed line is still discoverable
//                          without opening the diff view
//
// Design notes:
//
//   - Recompute on every transaction rather than debouncing. For
//     typical source-code sizes (`diffLines` on 3k-line files runs
//     in single-digit milliseconds) this is cheap enough; when the
//     line count genuinely gets pathological we'll move this to a
//     worker. Not today.
//   - `HEAD` content is reconfigured via a facet keyed by a
//     compartment on the Editor. Keeping it out of the StateField
//     directly means the Editor can update it in-place without
//     rebuilding state — same pattern the blame extension uses.
//   - Marker positioning is 1-based line numbers matching
//     CodeMirror's `doc.lineAt(...).number`. Sets, not arrays, so
//     `lineMarker` is O(1) per line regardless of the diff's shape.
//   - This is a read-only surface. Revert-hunk / stage-hunk UIs
//     belong in the SCM panel (Phase 5, later slice); the gutter
//     is an at-a-glance indicator, not an editing surface.

import { EditorSelection, Facet, StateField, type Extension, type Transaction } from '@codemirror/state';
import { EditorView, gutter, GutterMarker, ViewPlugin, type PluginValue, type ViewUpdate } from '@codemirror/view';
import { diffLines } from 'diff';

/**
 * Facet the Editor fills from `workspace.headByPath`. `null` means
 * "we have no `HEAD` content for this file" (outside a repo,
 * untracked, still loading, or the file is brand new) — the StateField
 * short-circuits to an empty classification and the gutter stays
 * blank. `combine` prefers the latest non-null contribution, mirroring
 * `blameFacet`'s pattern.
 */
export const headTextFacet = Facet.define<string | null, string | null>({
	combine: (values) => values.findLast((v) => v !== null) ?? null,
});

export type GitLineChanges = {
	/** 1-based line numbers for pure additions (not replacing a removal). */
	added: Set<number>;
	/** 1-based line numbers for added lines that directly replace removed ones. */
	modified: Set<number>;
	/**
	 * 1-based line numbers whose *top* edge should carry a "deletion above"
	 * wedge — the line itself still exists, but `HEAD` had extra lines
	 * immediately before it that are now gone. Points at where the
	 * cursor would land after an `undo`.
	 */
	deletedAbove: Set<number>;
	/**
	 * 1-based line numbers whose *bottom* edge should carry a wedge,
	 * used for the special case of a deletion at the very end of the
	 * file where there's no following line to anchor `deletedAbove`.
	 */
	deletedBelow: Set<number>;
};

const EMPTY_CHANGES: GitLineChanges = {
	added: new Set(),
	modified: new Set(),
	deletedAbove: new Set(),
	deletedBelow: new Set(),
};

/**
 * Walk `diffLines` output into per-line buckets. Adjacent
 * removed → added pairs fold into `modified` for the added side;
 * the removed side has no corresponding lines in the working tree
 * so nothing is emitted for it in that case.
 *
 * Pure removals (not adjacent to an addition) leave a deletion
 * wedge on the following line (or on the last line's bottom edge,
 * when the removal is at end-of-file).
 */
export function computeLineChanges(head: string, current: string): GitLineChanges {
	if (head === current) {
		return EMPTY_CHANGES;
	}
	const parts = diffLines(head, current);
	const added = new Set<number>();
	const modified = new Set<number>();
	const deletedAbove = new Set<number>();
	const deletedBelow = new Set<number>();
	let line = 1;
	for (let i = 0; i < parts.length; i++) {
		const part = parts[i];
		if (!part) {
			continue;
		}
		const count = part.count ?? 0;
		if (part.added) {
			const prev = parts[i - 1];
			const target = prev?.removed ? modified : added;
			for (let j = 0; j < count; j++) {
				target.add(line + j);
			}
			line += count;
		} else if (part.removed) {
			const next = parts[i + 1];
			if (!next?.added) {
				// Pure deletion. Anchor the wedge on whatever line
				// is at the current `line` pointer; if we're past
				// the last line of the working tree, fall back to
				// the previous line's bottom edge.
				if (line > current.split('\n').length) {
					deletedBelow.add(line - 1);
				} else {
					deletedAbove.add(line);
				}
			}
			// `removed` parts don't advance `line` — they're lines
			// that exist in HEAD but not in the working tree.
		} else {
			line += count;
		}
	}
	return { added, modified, deletedAbove, deletedBelow };
}

class GutterLineMarker extends GutterMarker {
	override readonly elementClass: string;

	constructor(className: string) {
		super();
		this.elementClass = className;
	}

	override eq(other: GutterMarker): boolean {
		return other instanceof GutterLineMarker && other.elementClass === this.elementClass;
	}
}

const ADDED_MARKER = new GutterLineMarker('cm-git-change cm-git-change-added');
const MODIFIED_MARKER = new GutterLineMarker('cm-git-change cm-git-change-modified');
const DELETED_ABOVE_MARKER = new GutterLineMarker('cm-git-change cm-git-change-deleted-above');
const DELETED_BELOW_MARKER = new GutterLineMarker('cm-git-change cm-git-change-deleted-below');
const ADDED_AND_DELETED_BELOW_MARKER = new GutterLineMarker(
	'cm-git-change cm-git-change-added cm-git-change-deleted-below',
);
const MODIFIED_AND_DELETED_BELOW_MARKER = new GutterLineMarker(
	'cm-git-change cm-git-change-modified cm-git-change-deleted-below',
);
// A single "empty but sized" marker stands in on rows that have no
// change. The gutter extension reserves width per the widest marker
// it's asked to render — without this spacer the gutter would
// collapse to 0px and reflow every time the first change appears.
const SPACER_MARKER = new GutterLineMarker('cm-git-change cm-git-change-spacer');

const gitChangesField = StateField.define<GitLineChanges>({
	create(state) {
		const head = state.facet(headTextFacet);
		if (head === null) {
			return EMPTY_CHANGES;
		}
		return computeLineChanges(head, state.doc.toString());
	},
	update(value, tr: Transaction) {
		const prevHead = tr.startState.facet(headTextFacet);
		const nextHead = tr.state.facet(headTextFacet);
		const headChanged = prevHead !== nextHead;
		if (!headChanged && !tr.docChanged) {
			return value;
		}
		if (nextHead === null) {
			return EMPTY_CHANGES;
		}
		return computeLineChanges(nextHead, tr.state.doc.toString());
	},
});

function markerFor(changes: GitLineChanges, lineNo: number): GutterMarker | null {
	const isAdded = changes.added.has(lineNo);
	const isModified = changes.modified.has(lineNo);
	const isDeletedAbove = changes.deletedAbove.has(lineNo);
	const isDeletedBelow = changes.deletedBelow.has(lineNo);
	// `deletedAbove` folds into the main status colour by virtue of
	// CSS — the `::before` wedge paints on the top edge regardless of
	// the row's main colour. Handling the common cases first keeps
	// the fall-through simple.
	if (isAdded && isDeletedBelow) {
		return ADDED_AND_DELETED_BELOW_MARKER;
	}
	if (isModified && isDeletedBelow) {
		return MODIFIED_AND_DELETED_BELOW_MARKER;
	}
	if (isAdded) {
		return ADDED_MARKER;
	}
	if (isModified) {
		return MODIFIED_MARKER;
	}
	if (isDeletedAbove) {
		return DELETED_ABOVE_MARKER;
	}
	if (isDeletedBelow) {
		return DELETED_BELOW_MARKER;
	}
	return null;
}

/**
 * Overview-ruler ViewPlugin: a thin strip pinned to the editor's
 * right edge that maps every git-change line onto a scaled-down
 * position, so the user can see *where* in the file changes live
 * without scrolling. Markers are clickable — dispatching a
 * `scrollIntoView` jumps the editor to that line, centred.
 *
 * The strip overlays the native scrollbar. `pointer-events: none`
 * on the container passes scrollbar drag / track-click through to
 * the browser, while each marker re-enables pointer events so it
 * can handle its own click. That gives us a discoverable indicator
 * without commandeering the scrollbar or stealing layout width.
 */
class GitOverviewPlugin implements PluginValue {
	private readonly overlay: HTMLDivElement;
	private readonly onClick: (event: MouseEvent) => void;
	private lastChanges: GitLineChanges | null = null;
	private lastLines = -1;

	constructor(private readonly view: EditorView) {
		this.overlay = document.createElement('div');
		this.overlay.className = 'cm-git-overview';
		// Mount under `.cm-editor` rather than `.cm-scroller` so the
		// strip stays anchored to the editor frame regardless of
		// scroll position — we want a global overview, not a
		// per-viewport indicator.
		view.dom.appendChild(this.overlay);
		this.onClick = (event) => this.handleClick(event);
		this.overlay.addEventListener('click', this.onClick);
		this.render();
	}

	update(update: ViewUpdate): void {
		// Only re-render when the underlying diff or line count
		// changes. `gitChangesField`'s `update` returns the previous
		// value by reference when there was no docChange or facet
		// change, so identity works as our "did anything move?"
		// check — avoids thrashing the DOM on every keystroke.
		const changes = update.state.field(gitChangesField, false) ?? null;
		const lines = update.state.doc.lines;
		if (changes === this.lastChanges && lines === this.lastLines) {
			return;
		}
		this.lastChanges = changes;
		this.lastLines = lines;
		this.render();
	}

	destroy(): void {
		this.overlay.removeEventListener('click', this.onClick);
		this.overlay.remove();
	}

	private render(): void {
		const changes = this.view.state.field(gitChangesField, false) ?? null;
		const lines = this.view.state.doc.lines;
		// Cheaper than `innerHTML = ''` and avoids the HTML-parsing
		// cost for each re-render, but functionally the same.
		while (this.overlay.firstChild) {
			this.overlay.removeChild(this.overlay.firstChild);
		}
		if (!changes || lines <= 0) {
			return;
		}
		const frag = document.createDocumentFragment();
		const paint = (set: Set<number>, cls: string) => {
			for (const lineNo of set) {
				const el = document.createElement('div');
				el.className = `cm-git-overview-marker ${cls}`;
				// Centre the marker on its line within the file's
				// vertical extent. `(lineNo - 0.5) / lines` gives
				// the fractional midline of a 1-based line number.
				el.style.top = `${((lineNo - 0.5) / lines) * 100}%`;
				el.dataset.line = String(lineNo);
				frag.appendChild(el);
			}
		};
		paint(changes.added, 'cm-git-overview-added');
		paint(changes.modified, 'cm-git-overview-modified');
		// Fold both deletion variants into one colour bucket; the
		// overview's job is "here's where deletions were", not
		// "distinguish above-from-below". The gutter already
		// differentiates them visually.
		paint(changes.deletedAbove, 'cm-git-overview-deleted');
		paint(changes.deletedBelow, 'cm-git-overview-deleted');
		this.overlay.appendChild(frag);
	}

	private handleClick(event: MouseEvent): void {
		const target = event.target;
		if (!(target instanceof HTMLElement)) {
			return;
		}
		const lineStr = target.dataset.line;
		if (lineStr === undefined) {
			return;
		}
		const lineNo = Number(lineStr);
		if (!Number.isFinite(lineNo) || lineNo < 1 || lineNo > this.view.state.doc.lines) {
			return;
		}
		const line = this.view.state.doc.line(lineNo);
		this.view.dispatch({
			selection: EditorSelection.cursor(line.from),
			effects: EditorView.scrollIntoView(line.from, { y: 'center' }),
		});
		this.view.focus();
	}
}

const gitOverviewPlugin = ViewPlugin.fromClass(GitOverviewPlugin);

export function gitChangesExtension(): Extension {
	return [
		gitChangesField,
		gutter({
			class: 'cm-git-changes-gutter',
			lineMarker(view, line) {
				const changes = view.state.field(gitChangesField, false);
				if (!changes) {
					return null;
				}
				const lineNo = view.state.doc.lineAt(line.from).number;
				return markerFor(changes, lineNo);
			},
			lineMarkerChange(update) {
				return update.startState.field(gitChangesField, false) !== update.state.field(gitChangesField, false);
			},
			// Keep a constant-width gutter so the first appearance of
			// a change doesn't nudge the editor content sideways.
			initialSpacer: () => SPACER_MARKER,
		}),
		gitOverviewPlugin,
	];
}
