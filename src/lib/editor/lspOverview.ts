// LSP-diagnostic overview ruler.
//
// Thin strip pinned to the editor's right edge (overlaying the native
// scrollbar) that plots every LSP diagnostic in the buffer at its
// scaled vertical position — same trick the git-changes overview
// ruler uses, but reading from `@codemirror/lint`'s state field
// instead of `gitChangesField`. Lets the user spot "is there an
// error further down this 3000-line file?" without scrolling.
//
// Layout coexists with the git overview by sitting in a separate
// vertical lane: git markers hug the right edge of the 10px strip
// (covering ~6px against the scrollbar), LSP markers paint just to
// their left in a parallel 4px lane. Two `position: absolute`
// containers stack on top of each other in z, but the markers
// inside use different `right` offsets so they never visually
// collide.
//
// Diagnostics come in via `setDiagnostics` (dispatched by
// `applyDiagnostics` in `lsp.ts`), which fires
// `setDiagnosticsEffect`. We watch for that effect, doc changes,
// and line-count changes; everything else short-circuits to avoid
// thrashing the DOM on every keystroke.

import { EditorSelection } from '@codemirror/state';
import { EditorView, ViewPlugin, type PluginValue, type ViewUpdate } from '@codemirror/view';
import { forEachDiagnostic, setDiagnosticsEffect, type Diagnostic as CmDiagnostic } from '@codemirror/lint';

import { overviewMountFacet } from './gitChanges';

type Severity = NonNullable<CmDiagnostic['severity']>;

type Mark = {
	/** 1-based line number the diagnostic anchors at. Centred on
	 * its midline in the overview. Multi-line ranges still pin to
	 * the start — matches how editor lint underlines are read. */
	line: number;
	severity: Severity;
};

/** Severity → CSS class on the marker. Mirrors the order in the
 * legend below the overview strip. */
const SEVERITY_CLASS: Record<Severity, string> = {
	error: 'cm-lsp-overview-error',
	warning: 'cm-lsp-overview-warning',
	info: 'cm-lsp-overview-info',
	hint: 'cm-lsp-overview-hint',
};

/** Severity ordering for paint priority — higher severities paint
 * last so they win when two diagnostics share a line. */
const SEVERITY_RANK: Record<Severity, number> = {
	hint: 0,
	info: 1,
	warning: 2,
	error: 3,
};

class LspOverviewPlugin implements PluginValue {
	private readonly overlay: HTMLDivElement;
	private readonly onClick: (event: MouseEvent) => void;
	private lastSignature = '';

	constructor(private readonly view: EditorView) {
		this.overlay = document.createElement('div');
		this.overlay.className = 'cm-lsp-overview';
		// Re-parent the same way `GitOverviewPlugin` does so the
		// strip lands on the scrolling layer, not inside the
		// scrolled content. The diff view fills `overviewMountFacet`
		// with a closure that returns `.diff-host`; the regular
		// editor leaves it null and we fall back to `.cm-editor`.
		const overrideMount = view.state.facet(overviewMountFacet);
		const mount = overrideMount?.(view) ?? view.dom;
		mount.appendChild(this.overlay);
		this.onClick = (event) => this.handleClick(event);
		this.overlay.addEventListener('click', this.onClick);
		this.render();
	}

	update(update: ViewUpdate): void {
		const diagnosticsChanged = update.transactions.some((tr) => tr.effects.some((e) => e.is(setDiagnosticsEffect)));
		if (!diagnosticsChanged && !update.docChanged && !update.viewportChanged) {
			return;
		}
		this.render();
	}

	destroy(): void {
		this.overlay.removeEventListener('click', this.onClick);
		this.overlay.remove();
	}

	private render(): void {
		const marks = collectMarks(this.view);
		const lines = this.view.state.doc.lines;
		// Cheap signature so we can short-circuit DOM work when
		// `update` was a false positive (e.g. an unrelated state
		// effect fired without changing the diagnostic list).
		// Sorting by line keeps the signature stable across map
		// iteration order changes.
		marks.sort((a, b) => a.line - b.line || SEVERITY_RANK[a.severity] - SEVERITY_RANK[b.severity]);
		const signature = `${lines}|${marks.map((m) => `${m.line}:${m.severity[0]}`).join(',')}`;
		if (signature === this.lastSignature) {
			return;
		}
		this.lastSignature = signature;
		while (this.overlay.firstChild) {
			this.overlay.removeChild(this.overlay.firstChild);
		}
		if (lines <= 0 || marks.length === 0) {
			return;
		}
		const frag = document.createDocumentFragment();
		// Lower severities first so higher ones overpaint where
		// they share a line — the user's eye should land on the
		// most severe marker.
		marks.sort((a, b) => SEVERITY_RANK[a.severity] - SEVERITY_RANK[b.severity]);
		for (const mark of marks) {
			const el = document.createElement('div');
			el.className = `cm-lsp-overview-marker ${SEVERITY_CLASS[mark.severity]}`;
			el.style.top = `${((mark.line - 0.5) / lines) * 100}%`;
			el.dataset.line = String(mark.line);
			el.title = mark.severity;
			frag.appendChild(el);
		}
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

function collectMarks(view: EditorView): Mark[] {
	const marks: Mark[] = [];
	forEachDiagnostic(view.state, (d, from) => {
		const severity = d.severity ?? 'info';
		const line = view.state.doc.lineAt(from).number;
		marks.push({ line, severity });
	});
	return marks;
}

/** ViewPlugin export for inclusion in the editor's extension list.
 * Self-contained — reads `@codemirror/lint`'s diagnostic state field
 * directly, so callers don't need to feed it anything. */
export const lspOverviewExtension = ViewPlugin.fromClass(LspOverviewPlugin);
