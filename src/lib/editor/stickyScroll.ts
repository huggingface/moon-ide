// VS Code-style "sticky scroll": pins the chain of enclosing
// definitions (class → method → inner fn) of the first visible line
// to the top of the editor, so a reader parked mid-function always
// knows which scope they're in. Works in the regular editor and in
// both panes of the diff view.
//
// Scope detection reuses `enclosingStack` from
// `diffCollapseContext.ts` — the same deliberately-heuristic
// indent-walk the Review tab uses to annotate collapse placeholders
// (see that module's header for why we don't ask the LSP). Never
// wrong, occasionally absent; an empty stack simply hides the
// header.
//
// DOM strategy: a zero-height `position: sticky; top: 0` wrapper
// prepended to `view.dom` (`.cm-editor`), with the rows absolutely
// positioned inside it.
//
//  - Regular editor: `.cm-editor` is the fixed-height box and
//    `.cm-scroller` scrolls inside it, so the wrapper never actually
//    sticks — it just sits at the editor's top edge and the rows
//    overlay the scroller. The rows are offset below any
//    `.cm-panels-top` (F2 rename) by measuring the scroller's top.
//  - Diff view: `@codemirror/merge` renders each `.cm-editor` at
//    natural (doc) height and scrolls the outer `.cm-mergeView`
//    (see `DiffView.svelte`'s synthetic h-scrollbar for the same
//    dance). The sticky chain walks up through the
//    `overflow: visible` column to `.cm-mergeView`, so the wrapper
//    pins to the merge view's viewport top per pane.
//
// Living inside `view.dom` (rather than a component-level overlay)
// buys three things for free: theme font/colors inherit from the
// `&` theme block, CM's scoped `.cm-line` padding applies to the
// row text so columns line up with real code, and the extension
// installs identically on every surface.

import { EditorView, ViewPlugin, type ViewUpdate } from '@codemirror/view';
import { highlightingFor, syntaxTree } from '@codemirror/language';
import { highlightTree, type Highlighter } from '@lezer/highlight';
import { enclosingStack, type EnclosingDef } from './diffCollapseContext';

// Deeper nesting than this is clipped to the innermost scopes —
// matching VS Code's default cap, and keeping the header from
// eating the viewport in deeply-indented code.
const MAX_ROWS = 5;

/** The element that actually scrolls vertically. Inside a merge
 *  view that's the outer `.cm-mergeView` — the package renders the
 *  per-pane scroller at natural height (`overflow-y: visible
 *  !important`), and a computed-style probe can't tell (specifying
 *  `visible` on one axis while the other is `auto` *computes* to
 *  `auto`). Everywhere else it's the pane's own `.cm-scroller`. */
function verticalScroller(view: EditorView): HTMLElement {
	return view.dom.closest<HTMLElement>('.cm-mergeView') ?? view.scrollDOM;
}

/** Paint one definition line with the editor's own highlight style
 *  (`highlightingFor` resolves to the same CSS-in-JS classes the
 *  live document uses, so colors are pixel-identical). Falls back
 *  to plain text when the syntax tree hasn't reached the line —
 *  header lines sit above the viewport, so in practice it has. */
function renderLineInto(view: EditorView, lineNumber: number, parent: HTMLElement): void {
	const line = view.state.doc.line(lineNumber);
	const text = line.text;
	let pos = line.from;
	const emit = (to: number, cls: string | null) => {
		if (to <= pos) {
			return;
		}
		const slice = text.slice(pos - line.from, to - line.from);
		if (cls === null || cls === '') {
			parent.appendChild(document.createTextNode(slice));
		} else {
			const span = document.createElement('span');
			span.className = cls;
			span.textContent = slice;
			parent.appendChild(span);
		}
		pos = to;
	};
	const tree = syntaxTree(view.state);
	if (tree.length >= line.to) {
		const highlighter: Highlighter = { style: (tags) => highlightingFor(view.state, tags) };
		highlightTree(
			tree,
			highlighter,
			(from, to, cls) => {
				emit(from, null);
				emit(to, cls);
			},
			line.from,
			line.to,
		);
	}
	emit(line.to, null);
}

class StickyScrollPlugin {
	private readonly view: EditorView;
	private wrapper: HTMLElement | null = null;
	private rows: HTMLElement | null = null;
	private scroller: HTMLElement | null = null;
	private resizeObserver: ResizeObserver | null = null;
	private raf = -1;
	private destroyed = false;
	// Row count from the previous pass — the iteration seed for the
	// "how many lines does the header itself cover?" fixed point.
	private rowCount = 0;
	// `line,line,…` of the last rendered stack; skips DOM rebuilds
	// while scrolling within one scope chain. Cleared on doc /
	// geometry changes so edits and theme flips re-render.
	private lastKey: string | null = null;

	private readonly onScroll = () => {
		this.schedule();
	};

	// The rows overlay the scroller but aren't its descendants, so a
	// wheel gesture over the header would otherwise scroll nothing
	// (regular editor) or chain to the wrong container. Forward it.
	private readonly onWheel = (event: WheelEvent) => {
		const scroller = this.scroller;
		if (scroller === null) {
			return;
		}
		event.preventDefault();
		const unit =
			event.deltaMode === 1 ? this.view.defaultLineHeight : event.deltaMode === 2 ? scroller.clientHeight : 1;
		scroller.scrollTop += event.deltaY * unit;
	};

	private readonly onClick = (event: MouseEvent) => {
		const target = event.target;
		if (!(target instanceof HTMLElement)) {
			return;
		}
		const row = target.closest<HTMLElement>('.cm-stickyScroll-row');
		if (row === null || row.dataset.line === undefined) {
			return;
		}
		const lineNumber = Number.parseInt(row.dataset.line, 10);
		if (!Number.isFinite(lineNumber) || lineNumber < 1 || lineNumber > this.view.state.doc.lines) {
			return;
		}
		const pos = this.view.state.doc.line(lineNumber).from;
		// Land the definition just below the ancestors that will
		// still be pinned above it (its index in the header), not
		// hidden behind the header itself.
		const index = Array.prototype.indexOf.call(row.parentElement?.children ?? [], row);
		this.view.dispatch({
			selection: { anchor: pos },
			effects: EditorView.scrollIntoView(pos, {
				y: 'start',
				yMargin: Math.max(0, index) * this.view.defaultLineHeight,
			}),
		});
		this.view.focus();
	};

	constructor(view: EditorView) {
		this.view = view;
		// Defer setup a frame: scroller detection depends on DOM
		// ancestry, and in the diff view the pane isn't parented
		// into `.cm-mergeView` until the MergeView constructor
		// finishes.
		requestAnimationFrame(() => {
			this.setup();
		});
	}

	update(update: ViewUpdate) {
		if (update.docChanged || update.viewportChanged || update.geometryChanged) {
			this.lastKey = null;
			this.schedule();
		}
	}

	destroy() {
		this.destroyed = true;
		if (this.raf !== -1) {
			cancelAnimationFrame(this.raf);
		}
		this.scroller?.removeEventListener('scroll', this.onScroll);
		this.resizeObserver?.disconnect();
		this.wrapper?.remove();
	}

	private setup(): void {
		if (this.destroyed || !this.view.dom.isConnected) {
			return;
		}
		const scroller = verticalScroller(this.view);
		this.scroller = scroller;
		const wrapper = document.createElement('div');
		wrapper.className = 'cm-stickyScroll';
		// Screen-reader / find-in-page noise, not content.
		wrapper.setAttribute('aria-hidden', 'true');
		const rows = document.createElement('div');
		rows.className = 'cm-stickyScroll-rows';
		rows.addEventListener('wheel', this.onWheel, { passive: false });
		rows.addEventListener('click', this.onClick);
		wrapper.appendChild(rows);
		this.view.dom.prepend(wrapper);
		this.wrapper = wrapper;
		this.rows = rows;
		scroller.addEventListener('scroll', this.onScroll, { passive: true });
		this.resizeObserver = new ResizeObserver(() => {
			this.schedule();
		});
		this.resizeObserver.observe(scroller);
		this.schedule();
	}

	private schedule(): void {
		if (this.raf !== -1) {
			return;
		}
		this.raf = requestAnimationFrame(() => {
			this.raf = -1;
			this.annotate();
		});
	}

	private annotate(): void {
		const { view, wrapper, rows, scroller } = this;
		if (this.destroyed || wrapper === null || rows === null || scroller === null || !view.dom.isConnected) {
			return;
		}
		const rowHeight = view.defaultLineHeight;
		const wrapperRect = wrapper.getBoundingClientRect();
		const scrollerRect = scroller.getBoundingClientRect();
		// Sit below any `.cm-panels-top` (regular editor: the wrapper
		// pins to `.cm-editor`'s top, the scroller starts lower). In
		// the diff view the wrapper sticks to the merge viewport
		// itself, so the offset clamps to 0.
		const topOffset = Math.max(0, scrollerRect.top - wrapperRect.top);
		rows.style.top = `${topOffset}px`;
		const headerTop = wrapperRect.top + topOffset;

		// Fixed point: the anchor is the first line visible *below*
		// the header, but the header's height is `stack.length` rows.
		// Seed with the previous count and iterate; converges in one
		// or two steps outside of pathological nesting.
		let stack: EnclosingDef[] = [];
		let count = this.rowCount;
		for (let i = 0; i < 4; i++) {
			const probeY = headerTop + count * rowHeight + 2;
			const block = view.lineBlockAtHeight(probeY - view.documentTop);
			const anchorLine = view.state.doc.lineAt(Math.min(block.from, view.state.doc.length)).number;
			stack = enclosingStack(view.state.doc, anchorLine);
			if (stack.length > MAX_ROWS) {
				stack = stack.slice(stack.length - MAX_ROWS);
			}
			if (stack.length === count) {
				break;
			}
			count = stack.length;
		}
		this.rowCount = stack.length;

		const key = stack.map((def) => def.line).join(',');
		if (key === this.lastKey) {
			return;
		}
		this.lastKey = key;
		rows.textContent = '';
		if (stack.length === 0) {
			rows.style.display = 'none';
			return;
		}
		rows.style.display = '';
		rows.style.lineHeight = `${rowHeight}px`;
		rows.style.tabSize = String(view.state.tabSize);

		// Mirror the gutter column so code in the header aligns with
		// code in the document. Measured (not themed) because gutter
		// width depends on the line-number digit count.
		const gutters = view.scrollDOM.querySelector('.cm-gutters');
		const guttersRect = gutters?.getBoundingClientRect() ?? null;
		const lineNumbers = gutters?.querySelector('.cm-lineNumbers');
		const numberPadRight =
			guttersRect !== null && lineNumbers !== null && lineNumbers !== undefined
				? Math.max(4, guttersRect.right - lineNumbers.getBoundingClientRect().right + 4)
				: 4;

		for (const def of stack) {
			const row = document.createElement('div');
			row.className = 'cm-stickyScroll-row';
			row.dataset.line = String(def.line);
			row.title = `Jump to line ${def.line}`;
			if (guttersRect !== null) {
				const num = document.createElement('span');
				num.className = 'cm-stickyScroll-num';
				num.style.width = `${guttersRect.width}px`;
				num.style.paddingRight = `${numberPadRight}px`;
				num.textContent = String(def.line);
				row.appendChild(num);
			}
			const code = document.createElement('span');
			code.className = 'cm-line cm-stickyScroll-code';
			renderLineInto(view, def.line, code);
			row.appendChild(code);
			rows.appendChild(row);
		}
	}
}

/**
 * Sticky enclosing-scope header. Install on the regular editor and
 * on each pane of a `MergeView`; the plugin adapts to whichever
 * element actually scrolls.
 */
export const stickyScrollExtension = ViewPlugin.fromClass(StickyScrollPlugin);
