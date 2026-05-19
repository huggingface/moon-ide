// Per-line git-change indicators on the editor's line-number gutter.
//
// Pulls the file's `HEAD` content via the workspace cache (populated
// by `WorkspaceState.refreshHead` when a buffer is opened) and, on
// every transaction, line-diffs it against the current doc. The
// resulting per-line classification is surfaced as a *line-number
// background tint* — GitHub-style — rather than a dedicated
// change-bar gutter:
//
//   - green `added`      — line exists in the working tree but not
//                          in `HEAD` and isn't adjacent to a removal
//   - blue  `modified`   — added block that directly replaces a
//                          removed block; the typical "I changed
//                          this line" case
//   - red   `deletedAbove` / `deletedBelow` — thin border on the top
//                          or bottom edge of the adjacent line's
//                          line-number cell, so a pure deletion is
//                          still visible without opening the diff
//                          view
//
// Why line-number tint instead of a dedicated wedge gutter:
//
//   - One fewer column means less horizontal noise on file types
//     (.md, .json, configs) where the change-bar would still be on
//     a single-line edit.
//   - The line-number cell is a stable target the eye already lands
//     on, so a tint is enough to surface a change without growing
//     the chrome.
//   - The same marker shape works for the diff view (`diffGutterTint`)
//     so both surfaces share visual vocabulary.
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

import { EditorSelection, Facet, RangeSet, StateField, type Extension, type Transaction } from '@codemirror/state';
import {
	EditorView,
	gutterLineClass,
	GutterMarker,
	ViewPlugin,
	type Command,
	type PluginValue,
	type ViewUpdate,
} from '@codemirror/view';
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

/**
 * Optional alternate parent for the overview-ruler overlay. The
 * default attaches to the editor frame (`view.dom` = `.cm-editor`),
 * which is the right thing in a regular Editor — the editor frame
 * is fixed to the viewport because `.cm-scroller` scrolls inside.
 *
 * In a `@codemirror/merge` `MergeView`, though, the package sets
 * `.cm-scroller { height: auto !important; overflow-y: visible
 * !important }` and lets the OUTER `.cm-mergeView` scroll. That
 * means `.cm-editor`'s height is the doc height, not the viewport
 * height, so `top:0; bottom:0; right:0` against it lays the strip
 * **inside** the scrolling content and the markers scroll with the
 * code. Wrong layer.
 *
 * The DiffView fills this facet with a closure that finds the
 * outer `.cm-mergeView`, so the overlay lands on the merge view's
 * actual scrollbar gutter and stays pinned to the visible
 * viewport. `null` (default) keeps the editor-frame attachment
 * for the regular editor case.
 */
export const overviewMountFacet = Facet.define<
	((view: EditorView) => HTMLElement | null) | null,
	((view: EditorView) => HTMLElement | null) | null
>({
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

/**
 * Bare-elementClass marker for the `gutterLineClass` facet. The
 * class string lands on every gutter row for the marked line; CSS
 * (`editor/theme.ts`) scopes the actual styling to the line-number
 * gutter via `.cm-gutter.cm-lineNumbers .cm-gutterElement.cm-gitline-…`.
 * `toDOM` is intentionally omitted — `gutterLineClass` markers
 * that define one would render in every gutter, which we don't
 * want.
 */
class GitLineClassMarker extends GutterMarker {
	override readonly elementClass: string;

	constructor(className: string) {
		super();
		this.elementClass = className;
	}

	override eq(other: GutterMarker): boolean {
		return other instanceof GitLineClassMarker && other.elementClass === this.elementClass;
	}
}

/// Cache markers by class-string so identical sets of classes
/// reuse the same `GutterMarker` instance — RangeSet uses `eq` to
/// dedupe redraws, and a fresh instance every recompute would
/// thrash CM's gutter diff.
const MARKER_CACHE = new Map<string, GitLineClassMarker>();
function gitLineMarker(classes: readonly string[]): GitLineClassMarker {
	const key = classes.join(' ');
	let m = MARKER_CACHE.get(key);
	if (m) {
		return m;
	}
	m = new GitLineClassMarker(key);
	MARKER_CACHE.set(key, m);
	return m;
}

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

/**
 * Build the gutter-class RangeSet for the current change set.
 * One marker per affected line, classes joined when the same line
 * carries multiple flags (e.g. an added line that also has a
 * deletion below it). Lines without a change get no marker, which
 * means the line-number cell stays at its default background.
 */
function buildGutterClassSet(
	changes: GitLineChanges,
	state: { doc: EditorView['state']['doc'] },
): RangeSet<GutterMarker> {
	const byLine = new Map<number, string[]>();
	const push = (lineNo: number, cls: string) => {
		const cur = byLine.get(lineNo);
		if (cur) {
			cur.push(cls);
		} else {
			byLine.set(lineNo, [cls]);
		}
	};
	for (const lineNo of changes.added) {
		push(lineNo, 'cm-gitline cm-gitline-added');
	}
	for (const lineNo of changes.modified) {
		push(lineNo, 'cm-gitline cm-gitline-modified');
	}
	for (const lineNo of changes.deletedAbove) {
		push(lineNo, 'cm-gitline-deleted-above');
	}
	for (const lineNo of changes.deletedBelow) {
		push(lineNo, 'cm-gitline-deleted-below');
	}
	if (byLine.size === 0) {
		return RangeSet.empty;
	}
	const doc = state.doc;
	const docLines = doc.lines;
	const ranges = [];
	for (const [lineNo, classes] of byLine) {
		// Defend against a stale range from a transaction that
		// hasn't been reclassified yet (shouldn't happen with the
		// `Facet.compute` keyed on the field, but cheap insurance).
		if (lineNo < 1 || lineNo > docLines) {
			continue;
		}
		const line = doc.line(lineNo);
		ranges.push(gitLineMarker(classes).range(line.from));
	}
	// `RangeSet.of(.., true)` sorts for us, so the input order from
	// the per-set iteration above doesn't have to be ascending.
	return RangeSet.of(ranges, true);
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
		// Default mount is the editor frame (`view.dom = .cm-editor`),
		// which sits at the viewport height in a regular Editor —
		// `.cm-scroller` scrolls inside `.cm-editor`, so the overlay
		// stays pinned to the viewport. `overviewMountFacet` lets a
		// host (the diff view) substitute a different parent when the
		// editor frame's height isn't the viewport height; without
		// that escape hatch the strip would scroll with the doc and
		// land in the wrong layer.
		const overrideMount = view.state.facet(overviewMountFacet);
		const mount = overrideMount?.(view) ?? view.dom;
		mount.appendChild(this.overlay);
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

/**
 * Wire up:
 *
 *   - the per-line change classification (`gitChangesField`)
 *   - the gutter-line-class facet that paints the line-number cell
 *     background per change kind (replaces the old wedge gutter —
 *     see the module docstring)
 *   - the right-edge overview ruler (unchanged)
 *
 * The previous `onGutterClick` "click the wedge to open diff mode"
 * affordance is gone with the wedge — the user already has
 * `Ctrl+Shift+D`, the per-tab toggle, and the SCM panel's diff
 * column for the same intent, and a click on the line-number cell
 * would collide with line-selection gestures other editors lean on.
 * Add it back via a synthetic transparent gutter if it's missed.
 */
export function gitChangesExtension(): Extension {
	return [
		gitChangesField,
		gutterLineClass.compute([gitChangesField], (state) => buildGutterClassSet(state.field(gitChangesField), state)),
		gitOverviewPlugin,
	];
}

/**
 * Sorted union of every line number that carries a git-change
 * marker (added / modified / deletion above / deletion below).
 * Used by the Alt-Up / Alt-Down navigation commands to step
 * between marked lines without caring which bucket they're in —
 * "next change" is the same gesture regardless of whether the
 * line is an addition, a modification, or anchors a deletion.
 */
function changeLines(changes: GitLineChanges): number[] {
	const all = new Set<number>();
	for (const n of changes.added) {
		all.add(n);
	}
	for (const n of changes.modified) {
		all.add(n);
	}
	for (const n of changes.deletedAbove) {
		all.add(n);
	}
	for (const n of changes.deletedBelow) {
		all.add(n);
	}
	return [...all].toSorted((a, b) => a - b);
}

/**
 * Jump the caret to `lineNo`, centring it in the viewport. Shared
 * by the next / previous commands and the overview-ruler click
 * handler — same dispatch shape both places.
 */
function jumpToLine(view: EditorView, lineNo: number): void {
	const line = view.state.doc.line(lineNo);
	view.dispatch({
		selection: EditorSelection.cursor(line.from),
		effects: EditorView.scrollIntoView(line.from, { y: 'center' }),
	});
}

/**
 * Step the caret to the next git-change line below the current
 * caret. Returns `true` when the `gitChangesField` is installed
 * (i.e. this editor has the gitChanges extension) regardless of
 * whether there's somewhere to jump to — the binding deliberately
 * shadows CodeMirror's default `Alt-ArrowDown` (`moveLineDown`)
 * even on a clean buffer; the team wants Alt+Down to mean
 * "next change", not "move line", everywhere.
 *
 * Returns `false` when the field isn't installed so the binding
 * falls through to whatever else is bound (host-level handlers,
 * other keymaps). That keeps the command harmless when reused on
 * a surface that doesn't carry git-change data.
 */
export const goToNextChange: Command = (view) => {
	const changes = view.state.field(gitChangesField, false);
	if (!changes) {
		return false;
	}
	const lines = changeLines(changes);
	if (lines.length === 0) {
		return true;
	}
	const caretLine = view.state.doc.lineAt(view.state.selection.main.head).number;
	const next = lines.find((n) => n > caretLine);
	if (next === undefined) {
		return true;
	}
	jumpToLine(view, next);
	return true;
};

/**
 * Step the caret to the previous git-change line above the
 * current caret. Mirror of `goToNextChange` — see that doc for
 * the return-value contract.
 */
export const goToPreviousChange: Command = (view) => {
	const changes = view.state.field(gitChangesField, false);
	if (!changes) {
		return false;
	}
	const lines = changeLines(changes);
	if (lines.length === 0) {
		return true;
	}
	const caretLine = view.state.doc.lineAt(view.state.selection.main.head).number;
	let prev: number | undefined;
	for (const n of lines) {
		if (n < caretLine) {
			prev = n;
		} else {
			break;
		}
	}
	if (prev === undefined) {
		return true;
	}
	jumpToLine(view, prev);
	return true;
};
