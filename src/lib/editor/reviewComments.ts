// CodeMirror extension for inline review comments on a review
// section's diff (Phase 5.7). See `specs/review-comments.md`.
//
// What it does:
//
//   - Renders each anchored comment as a block-widget card below
//     its anchored line(s): author, relative time, markdown-ish
//     body, edit / delete controls.
//   - Re-anchors comments by content on every doc change. The
//     stored line numbers are a hint; the comment's content
//     `fingerprint` is the truth. If the line at the hint still
//     matches, render there; otherwise scan a small window for the
//     fingerprint and re-pin (reporting the new line via `onRepin`);
//     if it can't be found at all, the card renders in a muted
//     "stale" state — still editable / deletable, never dropped.
//   - Hosts an inline composer card (open / cancel / submit) at a
//     requested line range, driven by a separate facet so the
//     section can open it from a keybinding or a gutter affordance.
//
// What it deliberately doesn't do:
//
//   - Resolve threads or show replies. A comment is a one-shot
//     local draft (see the spec's non-goals).
//   - Render full markdown. The body is shown as plain text with
//     line breaks preserved; rich rendering can come later if the
//     team asks. Keeping it text-only avoids pulling the markdown
//     pipeline into a hot per-keystroke decoration rebuild.
//
// Modelled on `conflictMarkers.ts`: a `Facet` carries reactive
// input (the comment list, the callbacks, the open-composer
// request), a `StateField` builds the block-widget `DecorationSet`,
// and `WidgetType`s render the DOM.

import {
	Compartment,
	Facet,
	StateEffect,
	StateField,
	type EditorState,
	type Extension,
	type Range,
} from '@codemirror/state';
import {
	Decoration,
	EditorView,
	gutter,
	GutterMarker,
	keymap,
	ViewPlugin,
	WidgetType,
	type DecorationSet,
} from '@codemirror/view';
import { getCachedMarkdown, renderMarkdown } from '../markdown';
import type { ReviewComment, ReviewSide } from '../protocol';

/** How far from the hint line to scan for a drifted fingerprint. */
const ANCHOR_SEARCH_RADIUS = 40;

/**
 * Content fingerprint for an anchored line range. Must match the
 * backend / state-layer implementation in `state.svelte.ts`
 * (`reviewFingerprint`) byte-for-byte so a fingerprint written at
 * comment-creation time re-resolves here: trim each line, join with
 * `\n`, FNV-1a 32-bit.
 */
export function reviewLineFingerprint(lineText: string): string {
	const normalized = lineText
		.split('\n')
		.map((l) => l.trim())
		.join('\n');
	let hash = 0x811c9dc5;
	for (let i = 0; i < normalized.length; i++) {
		hash ^= normalized.charCodeAt(i);
		hash = Math.imul(hash, 0x01000193);
	}
	return (hash >>> 0).toString(16).padStart(8, '0');
}

/** Trimmed-and-joined text of lines `start..=end` (1-based, clamped). */
function lineRangeText(state: EditorState, start: number, end: number): string | null {
	const doc = state.doc;
	if (start < 1 || end > doc.lines || start > end) {
		return null;
	}
	const parts: string[] = [];
	for (let n = start; n <= end; n++) {
		parts.push(doc.line(n).text);
	}
	return parts.join('\n');
}

/**
 * Re-locate a comment's anchor in the current doc by fingerprint.
 * Returns the 1-based `[startLine, endLine]` where it now sits, or
 * `null` if its content can't be found within the search window —
 * the "stale" case.
 */
function resolveAnchor(state: EditorState, comment: ReviewComment): [number, number] | null {
	const span = comment.anchor.endLine - comment.anchor.startLine;
	const fp = comment.anchor.fingerprint;
	// Hot path: the hint line range still matches.
	const atHint = lineRangeText(state, comment.anchor.startLine, comment.anchor.endLine);
	if (atHint !== null && reviewLineFingerprint(atHint) === fp) {
		return [comment.anchor.startLine, comment.anchor.endLine];
	}
	// Scan outward from the hint for a run of `span + 1` lines that
	// fingerprints to the same value. Nearest match wins.
	const doc = state.doc;
	for (let delta = 1; delta <= ANCHOR_SEARCH_RADIUS; delta++) {
		for (const dir of [-1, 1]) {
			const start = comment.anchor.startLine + dir * delta;
			const end = start + span;
			if (start < 1 || end > doc.lines) {
				continue;
			}
			const text = lineRangeText(state, start, end);
			if (text !== null && reviewLineFingerprint(text) === fp) {
				return [start, end];
			}
		}
	}
	return null;
}

/** Callbacks the section wires so the widgets can mutate workspace state. */
export type ReviewCommentCallbacks = {
	/** Persist a new comment from the composer. */
	onSubmit: (args: { startLine: number; endLine: number; lineText: string; body: string }) => void;
	/** Update an existing comment's body. */
	onEdit: (id: string, body: string) => void;
	/** Delete a comment. */
	onDelete: (id: string) => void;
	/** Close the composer (clears the open-composer request). */
	onCloseComposer: () => void;
	/** Open the composer anchored at a single 1-based line (gutter +). */
	onAddAtLine: (line: number) => void;
};

/** The comments anchored to this editor's side, in creation order. */
export const reviewCommentsFacet = Facet.define<readonly ReviewComment[], readonly ReviewComment[]>({
	combine: (values) => values[0] ?? [],
});

/** The section's callbacks. Exactly one provider per editor. */
export const reviewCallbacksFacet = Facet.define<ReviewCommentCallbacks, ReviewCommentCallbacks | null>({
	combine: (values) => values[0] ?? null,
});

/**
 * An open composer request: the 1-based line range to anchor a new
 * comment to, or `null` for "no composer open". The section sets
 * this from a keybinding / gutter click using the current
 * selection.
 */
export const reviewComposerFacet = Facet.define<
	{ startLine: number; endLine: number } | null,
	{
		startLine: number;
		endLine: number;
	} | null
>({
	combine: (values) => values[0] ?? null,
});

/**
 * Render `source` as markdown into `el`. Output of `renderMarkdown`
 * is DOMPurified upstream (see `markdown.ts`), so assigning it to
 * `innerHTML` is safe. Uses the sync cache when warm (no flash on
 * re-render); otherwise sets the raw text first and swaps in the
 * rendered HTML when the async render resolves.
 */
function renderMarkdownInto(el: HTMLElement, source: string): void {
	const cached = getCachedMarkdown(source);
	if (cached !== undefined) {
		el.innerHTML = cached;
		return;
	}
	el.textContent = source;
	void renderMarkdown(source).then((html) => {
		// The widget may have been torn down before the render
		// resolved; guard against writing into a detached node.
		if (el.isConnected) {
			el.innerHTML = html;
		}
	});
}

function relativeTime(iso: string): string {
	const then = Date.parse(iso);
	if (Number.isNaN(then)) {
		return '';
	}
	const secs = Math.max(0, Math.round((Date.now() - then) / 1000));
	if (secs < 60) {
		return 'just now';
	}
	const mins = Math.round(secs / 60);
	if (mins < 60) {
		return `${mins}m ago`;
	}
	const hours = Math.round(mins / 60);
	if (hours < 24) {
		return `${hours}h ago`;
	}
	const days = Math.round(hours / 24);
	return `${days}d ago`;
}

class CommentCardWidget extends WidgetType {
	constructor(
		private readonly comment: ReviewComment,
		private readonly stale: boolean,
		private readonly callbacks: ReviewCommentCallbacks,
	) {
		super();
	}

	override eq(other: WidgetType): boolean {
		return (
			other instanceof CommentCardWidget &&
			other.comment.id === this.comment.id &&
			other.comment.body === this.comment.body &&
			other.stale === this.stale
		);
	}

	toDOM(view: EditorView): HTMLElement {
		const root = document.createElement('div');
		root.className = this.stale ? 'cm-review-card cm-review-card-stale' : 'cm-review-card';
		root.contentEditable = 'false';

		const head = document.createElement('div');
		head.className = 'cm-review-card-head';
		const who = document.createElement('span');
		who.className = 'cm-review-card-author';
		who.textContent = 'You';
		head.appendChild(who);
		const when = document.createElement('span');
		when.className = 'cm-review-card-time';
		when.textContent = relativeTime(this.comment.createdAt);
		head.appendChild(when);
		if (this.stale) {
			const badge = document.createElement('span');
			badge.className = 'cm-review-card-staleflag';
			badge.textContent = 'line changed';
			badge.title = 'The line this comment was anchored to has changed; it may not publish to the right place.';
			head.appendChild(badge);
		}
		const spacer = document.createElement('span');
		spacer.className = 'cm-review-card-spacer';
		head.appendChild(spacer);
		head.appendChild(this.iconButton('Edit', 'Edit comment', () => this.beginEdit(view, root)));
		head.appendChild(this.iconButton('Delete', 'Delete comment', () => this.callbacks.onDelete(this.comment.id)));
		root.appendChild(head);

		const body = document.createElement('div');
		body.className = 'cm-review-card-body cm-review-markdown';
		renderMarkdownInto(body, this.comment.body);
		root.appendChild(body);

		// Stop CodeMirror from treating clicks inside the card as
		// text-selection gestures.
		root.addEventListener('mousedown', (e) => e.stopPropagation());
		return root;
	}

	private iconButton(label: string, title: string, handler: () => void): HTMLButtonElement {
		const btn = document.createElement('button');
		btn.type = 'button';
		btn.className = 'cm-review-card-btn';
		btn.textContent = label;
		btn.title = title;
		btn.addEventListener('mousedown', (e) => e.preventDefault());
		btn.addEventListener('click', (e) => {
			e.stopPropagation();
			handler();
		});
		return btn;
	}

	private beginEdit(view: EditorView, root: HTMLElement) {
		const editor = buildComposerForm(this.comment.body, 'Save', {
			onCancel: () => {
				// Re-render from state by nudging a no-op selection so
				// the StateField rebuilds the card.
				view.dispatch({ selection: view.state.selection });
			},
			onSubmit: (text) => {
				this.callbacks.onEdit(this.comment.id, text);
			},
		});
		root.replaceChildren(editor.root);
		editor.textarea.focus();
	}

	override ignoreEvent(): boolean {
		return true;
	}
}

class ComposerWidget extends WidgetType {
	constructor(
		private readonly startLine: number,
		private readonly endLine: number,
		private readonly callbacks: ReviewCommentCallbacks,
	) {
		super();
	}

	override eq(other: WidgetType): boolean {
		return other instanceof ComposerWidget && other.startLine === this.startLine && other.endLine === this.endLine;
	}

	toDOM(view: EditorView): HTMLElement {
		const lineText = lineRangeText(view.state, this.startLine, this.endLine) ?? '';
		const form = buildComposerForm('', 'Comment', {
			onCancel: () => this.callbacks.onCloseComposer(),
			onSubmit: (text) => {
				this.callbacks.onSubmit({
					startLine: this.startLine,
					endLine: this.endLine,
					lineText,
					body: text,
				});
				this.callbacks.onCloseComposer();
			},
		});
		// Focus after the widget is attached to the DOM.
		queueMicrotask(() => form.textarea.focus());
		return form.root;
	}

	override ignoreEvent(): boolean {
		return true;
	}
}

/** Shared composer form (used by both the new-comment and edit flows). */
function buildComposerForm(
	initial: string,
	submitLabel: string,
	handlers: { onCancel: () => void; onSubmit: (text: string) => void },
): { root: HTMLElement; textarea: HTMLTextAreaElement } {
	const root = document.createElement('div');
	root.className = 'cm-review-composer';
	root.contentEditable = 'false';
	root.addEventListener('mousedown', (e) => e.stopPropagation());

	const textarea = document.createElement('textarea');
	textarea.className = 'cm-review-composer-input';
	textarea.value = initial;
	textarea.rows = 3;
	textarea.placeholder = 'Leave a review comment…';
	root.appendChild(textarea);

	const actions = document.createElement('div');
	actions.className = 'cm-review-composer-actions';
	const cancel = document.createElement('button');
	cancel.type = 'button';
	cancel.className = 'cm-review-composer-btn';
	cancel.textContent = 'Cancel';
	cancel.addEventListener('mousedown', (e) => e.preventDefault());
	cancel.addEventListener('click', (e) => {
		e.stopPropagation();
		handlers.onCancel();
	});
	const submit = document.createElement('button');
	submit.type = 'button';
	submit.className = 'cm-review-composer-btn cm-review-composer-submit';
	submit.textContent = submitLabel;
	submit.addEventListener('mousedown', (e) => e.preventDefault());
	submit.addEventListener('click', (e) => {
		e.stopPropagation();
		const text = textarea.value.trim();
		if (text.length > 0) {
			handlers.onSubmit(text);
		}
	});
	actions.appendChild(cancel);
	actions.appendChild(submit);
	root.appendChild(actions);

	// Cmd/Ctrl+Enter submits, Escape cancels — standard composer keys.
	textarea.addEventListener('keydown', (e) => {
		if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) {
			e.preventDefault();
			e.stopPropagation();
			const text = textarea.value.trim();
			if (text.length > 0) {
				handlers.onSubmit(text);
			}
		} else if (e.key === 'Escape') {
			e.preventDefault();
			e.stopPropagation();
			handlers.onCancel();
		}
	});

	return { root, textarea };
}

function buildDecorations(state: EditorState): DecorationSet {
	const callbacks = state.facet(reviewCallbacksFacet);
	if (callbacks === null) {
		return Decoration.none;
	}
	const comments = state.facet(reviewCommentsFacet);
	const composer = state.facet(reviewComposerFacet);
	const ranges: Range<Decoration>[] = [];

	for (const comment of comments) {
		const resolved = resolveAnchor(state, comment);
		const stale = resolved === null;
		// Anchor the card below the resolved line (or the hint line,
		// clamped, when stale so it still renders somewhere sensible).
		// Re-pinning the persisted hint is a separate, non-render-path
		// concern (`reanchorComments`); the fingerprint re-resolves
		// every build regardless, so rendering never depends on the
		// stored hint being fresh.
		const lineNo = resolved !== null ? resolved[1] : Math.min(Math.max(1, comment.anchor.endLine), state.doc.lines);
		const line = state.doc.line(lineNo);
		ranges.push(
			Decoration.widget({
				widget: new CommentCardWidget(comment, stale, callbacks),
				side: 1,
				block: true,
			}).range(line.to),
		);
	}

	if (composer !== null && composer.startLine >= 1 && composer.endLine <= state.doc.lines) {
		const line = state.doc.line(Math.min(composer.endLine, state.doc.lines));
		ranges.push(
			Decoration.widget({
				widget: new ComposerWidget(composer.startLine, composer.endLine, callbacks),
				side: 1,
				block: true,
			}).range(line.to),
		);
	}

	// `sort: true` lets CodeMirror order the ranges (block widgets
	// can share an anchor position with the composer; CM resolves the
	// ordering by `side`).
	return Decoration.set(ranges, true);
}

const reviewCommentsField = StateField.define<DecorationSet>({
	create: (state) => buildDecorations(state),
	update: (value, tr) => {
		const facetsChanged =
			tr.startState.facet(reviewCommentsFacet) !== tr.state.facet(reviewCommentsFacet) ||
			tr.startState.facet(reviewComposerFacet) !== tr.state.facet(reviewComposerFacet) ||
			tr.startState.facet(reviewCallbacksFacet) !== tr.state.facet(reviewCallbacksFacet);
		if (!tr.docChanged && !facetsChanged) {
			return value;
		}
		return buildDecorations(tr.state);
	},
	provide: (f) => EditorView.decorations.from(f),
});

// --- Hover gutter "+" -------------------------------------------------
// A dedicated gutter column that shows a clickable "+" only on the
// line the pointer is currently over, so a reviewer can start a
// comment without first selecting text. The hovered line is tracked
// in a StateField updated by a mouse-move DOM handler.

const setHoverLine = StateEffect.define<number | null>();

const hoverLineField = StateField.define<number | null>({
	create: () => null,
	update: (value, tr) => {
		for (const e of tr.effects) {
			if (e.is(setHoverLine)) {
				return e.value;
			}
		}
		// A doc change can shift which line is under the (unmoved)
		// pointer; clear so a stale "+" doesn't linger on the wrong row.
		return tr.docChanged ? null : value;
	},
});

class AddCommentMarker extends GutterMarker {
	constructor(
		private readonly line: number,
		private readonly callbacks: ReviewCommentCallbacks,
	) {
		super();
	}

	override eq(other: GutterMarker): boolean {
		return other instanceof AddCommentMarker && other.line === this.line;
	}

	override toDOM(): HTMLElement {
		const btn = document.createElement('button');
		btn.type = 'button';
		btn.className = 'cm-review-add-btn';
		btn.textContent = '+';
		btn.title = 'Add a review comment on this line';
		btn.addEventListener('mousedown', (e) => e.preventDefault());
		btn.addEventListener('click', (e) => {
			e.stopPropagation();
			this.callbacks.onAddAtLine(this.line);
		});
		return btn;
	}
}

const addCommentGutter = gutter({
	class: 'cm-review-add-gutter',
	lineMarker: (view, lineBlock) => {
		const callbacks = view.state.facet(reviewCallbacksFacet);
		const hovered = view.state.field(hoverLineField, false) ?? null;
		if (callbacks === null || hovered === null) {
			return null;
		}
		const line = view.state.doc.lineAt(lineBlock.from).number;
		return line === hovered ? new AddCommentMarker(line, callbacks) : null;
	},
	// Recompute markers whenever the hovered line changes.
	lineMarkerChange: (update) =>
		update.startState.field(hoverLineField, false) !== update.state.field(hoverLineField, false),
});

// Track the pointer and publish the hovered line into the field.
// Listens on the editor's *outer* DOM (`view.dom`), not the content
// element `EditorView.domEventHandlers` would target: the "+" lives
// in a gutter, and a content-scoped `mouseleave` would fire the
// moment the pointer travels from the code toward the button —
// clearing the marker out from under the click. The outer element
// covers gutters + content, so the marker survives the trip and
// also appears when the pointer enters from the gutter side.
const hoverTracker = ViewPlugin.define((view) => {
	const move = (event: MouseEvent) => {
		const pos = view.posAtCoords({ x: event.clientX, y: event.clientY }, false);
		const line = view.state.doc.lineAt(pos).number;
		if (view.state.field(hoverLineField, false) !== line) {
			view.dispatch({ effects: setHoverLine.of(line) });
		}
	};
	const leave = () => {
		if (view.state.field(hoverLineField, false) !== null) {
			view.dispatch({ effects: setHoverLine.of(null) });
		}
	};
	view.dom.addEventListener('mousemove', move);
	view.dom.addEventListener('mouseleave', leave);
	return {
		destroy() {
			view.dom.removeEventListener('mousemove', move);
			view.dom.removeEventListener('mouseleave', leave);
		},
	};
});

// Styling for the cards, composer, and hover gutter. Lives on the
// extension (not in any host component's CSS) so every surface that
// wires review comments — review sections, the regular editor, the
// diff view — renders them identically. Colors ride the app's
// `--m-*` custom properties, which cascade from the document root.
//
// The add-comment gutter is given a **fixed width**: gutters size to
// their content, and the "+" marker only exists on the hovered line,
// so without the reservation the column would appear/disappear on
// hover and shift the code horizontally (the original bug). 14px is
// reserved once at mount and never changes.
const reviewBaseTheme = EditorView.baseTheme({
	'.cm-review-add-gutter': {
		width: '14px',
	},
	'.cm-review-add-btn': {
		display: 'inline-flex',
		alignItems: 'center',
		justifyContent: 'center',
		width: '13px',
		height: '13px',
		padding: '0',
		margin: '1px 0 0 1px',
		background: 'var(--m-accent, #4ec9b0)',
		border: 'none',
		borderRadius: '3px',
		color: 'var(--m-bg)',
		fontSize: '11px',
		fontWeight: '700',
		lineHeight: '1',
		cursor: 'pointer',
	},
	'.cm-review-card, .cm-review-composer': {
		margin: '4px 8px 4px 28px',
		border: '1px solid var(--m-border)',
		borderRadius: '6px',
		background: 'var(--m-bg-1)',
		fontFamily: 'var(--m-font-sans, system-ui, sans-serif)',
		fontSize: '12px',
		overflow: 'hidden',
	},
	'.cm-review-card-stale': {
		opacity: '0.7',
		borderStyle: 'dashed',
	},
	'.cm-review-card-head': {
		display: 'flex',
		alignItems: 'center',
		gap: '8px',
		padding: '5px 8px',
		borderBottom: '1px solid var(--m-border)',
		background: 'var(--m-bg-2, var(--m-bg-1))',
	},
	'.cm-review-card-author': {
		fontWeight: '600',
		color: 'var(--m-fg)',
	},
	'.cm-review-card-time': {
		color: 'var(--m-fg-muted)',
		fontSize: '11px',
	},
	'.cm-review-card-staleflag': {
		color: 'var(--m-git-modified, #e2c08d)',
		fontSize: '10px',
		textTransform: 'uppercase',
		letterSpacing: '0.03em',
	},
	'.cm-review-card-spacer': {
		flex: '1',
	},
	'.cm-review-card-btn': {
		padding: '1px 6px',
		background: 'transparent',
		border: '1px solid transparent',
		borderRadius: '4px',
		color: 'var(--m-fg-muted)',
		fontSize: '11px',
		cursor: 'pointer',
	},
	'.cm-review-card-btn:hover': {
		color: 'var(--m-fg)',
		borderColor: 'var(--m-border)',
	},
	'.cm-review-card-body': {
		padding: '6px 8px',
		color: 'var(--m-fg)',
		wordBreak: 'break-word',
		lineHeight: '1.4',
	},
	'.cm-review-markdown > :first-child': {
		marginTop: '0',
	},
	'.cm-review-markdown > :last-child': {
		marginBottom: '0',
	},
	'.cm-review-markdown p': {
		margin: '0 0 6px',
	},
	'.cm-review-markdown pre': {
		margin: '6px 0',
		padding: '6px 8px',
		background: 'var(--m-bg)',
		border: '1px solid var(--m-border)',
		borderRadius: '4px',
		overflowX: 'auto',
	},
	'.cm-review-markdown code': {
		fontFamily: 'var(--m-font-mono, monospace)',
		fontSize: '11px',
	},
	'.cm-review-composer': {
		padding: '6px',
	},
	'.cm-review-composer-input': {
		display: 'block',
		width: '100%',
		boxSizing: 'border-box',
		resize: 'vertical',
		padding: '6px 8px',
		background: 'var(--m-bg)',
		border: '1px solid var(--m-border)',
		borderRadius: '4px',
		color: 'var(--m-fg)',
		fontFamily: 'var(--m-font-sans, system-ui, sans-serif)',
		fontSize: '12px',
		lineHeight: '1.4',
	},
	'.cm-review-composer-input:focus': {
		outline: 'none',
		borderColor: 'var(--m-accent, #4ec9b0)',
	},
	'.cm-review-composer-actions': {
		display: 'flex',
		justifyContent: 'flex-end',
		gap: '6px',
		marginTop: '6px',
	},
	'.cm-review-composer-btn': {
		padding: '3px 10px',
		background: 'var(--m-bg-1)',
		border: '1px solid var(--m-border)',
		borderRadius: '4px',
		color: 'var(--m-fg)',
		fontSize: '11px',
		cursor: 'pointer',
	},
	'.cm-review-composer-btn:hover': {
		borderColor: 'var(--m-fg-muted)',
	},
	'.cm-review-composer-submit': {
		background: 'var(--m-accent, #4ec9b0)',
		borderColor: 'var(--m-accent, #4ec9b0)',
		color: 'var(--m-bg)',
		fontWeight: '600',
	},
});

/**
 * The review-comments CM extension. Most hosts should use
 * [`ReviewWiring`] rather than assembling the facets by hand.
 */
export function reviewCommentsExtension(): Extension {
	return [reviewCommentsField, hoverLineField, addCommentGutter, hoverTracker, reviewBaseTheme];
}

/** Operations a host wires into a [`ReviewWiring`]. */
export type ReviewWiringOps = {
	/** Persist a new comment (the host adds `path` and stores it). */
	add: (args: {
		side: ReviewSide;
		startLine: number;
		endLine: number;
		lineText: string;
		baselineRev: string;
		body: string;
	}) => void;
	edit: (id: string, body: string) => void;
	remove: (id: string) => void;
	/** The merge-base / HEAD SHA the host is currently reviewing against. */
	baselineRev: () => string;
};

/**
 * Per-editor controller that owns the comment-list / composer
 * compartments and the open-composer state, so each host component
 * (review section, editor, diff view) doesn't re-implement the same
 * ~60 lines of glue. Framework-free: the host attaches the live
 * `EditorView` once it exists and pushes reactive comment-list
 * updates through [`ReviewWiring.syncComments`].
 */
export class ReviewWiring {
	private readonly commentsComp = new Compartment();
	private readonly composerComp = new Compartment();
	private view: EditorView | null = null;

	constructor(
		private readonly side: ReviewSide,
		private readonly ops: ReviewWiringOps,
	) {}

	/**
	 * The full extension bundle for one editor: decorations + hover
	 * gutter + facets + the `Mod-Alt-c` open-composer keybinding.
	 * Safe to rebuild (e.g. when the host's state is recreated per
	 * file); the compartments are reused across rebuilds.
	 */
	extension(initialComments: readonly ReviewComment[]): Extension {
		const callbacks: ReviewCommentCallbacks = {
			onSubmit: ({ startLine, endLine, lineText, body }) => {
				this.ops.add({
					side: this.side,
					startLine,
					endLine,
					lineText,
					baselineRev: this.ops.baselineRev(),
					body,
				});
			},
			onEdit: (id, body) => this.ops.edit(id, body),
			onDelete: (id) => this.ops.remove(id),
			onCloseComposer: () => this.setComposer(null),
			onAddAtLine: (line) => this.setComposer({ startLine: line, endLine: line }),
		};
		return [
			reviewCommentsExtension(),
			reviewCallbacksFacet.of(callbacks),
			this.commentsComp.of(reviewCommentsFacet.of(initialComments)),
			this.composerComp.of(reviewComposerFacet.of(null)),
			keymap.of([
				{
					key: 'Mod-Alt-c',
					run: (view) => {
						this.openFromSelection(view);
						return true;
					},
				},
			]),
		];
	}

	/** Point the wiring at the live editor (idempotent). */
	attach(view: EditorView): void {
		this.view = view;
	}

	/** Drop the view reference on host teardown. */
	detach(): void {
		this.view = null;
	}

	/** Push a fresh comment list into the editor (reactive sync). */
	syncComments(comments: readonly ReviewComment[]): void {
		this.view?.dispatch({ effects: this.commentsComp.reconfigure(reviewCommentsFacet.of(comments)) });
	}

	/** Open the composer at the current selection's line range. */
	openFromSelection(view: EditorView): void {
		const sel = view.state.selection.main;
		const fromLine = view.state.doc.lineAt(sel.from).number;
		const toLineInfo = view.state.doc.lineAt(sel.to);
		// Same end-of-line snap as the selection publishers: a drag
		// ending at the very start of a line didn't mean to include it.
		const toLine =
			sel.to === toLineInfo.from && toLineInfo.number > fromLine ? toLineInfo.number - 1 : toLineInfo.number;
		this.setComposer({ startLine: fromLine, endLine: toLine });
	}

	private setComposer(req: { startLine: number; endLine: number } | null): void {
		this.view?.dispatch({ effects: this.composerComp.reconfigure(reviewComposerFacet.of(req)) });
	}
}

/** Filter a comment list to one diff side. */
export function commentsForSide(comments: readonly ReviewComment[], side: ReviewSide): readonly ReviewComment[] {
	return comments.filter((c) => c.anchor.side === side);
}

/**
 * Recompute hint line ranges for `comments` against `state` and
 * return only those whose hint actually moved (found at a new line
 * via fingerprint). The section persists these so the stored hint
 * stays fresh across launches — purely an optimization, since
 * rendering re-resolves by fingerprint every build. Comments that
 * went stale (fingerprint not found) are left untouched.
 */
export function reanchorComments(
	state: EditorState,
	comments: readonly ReviewComment[],
): { id: string; startLine: number; endLine: number }[] {
	const out: { id: string; startLine: number; endLine: number }[] = [];
	for (const comment of comments) {
		const resolved = resolveAnchor(state, comment);
		if (resolved !== null && (resolved[0] !== comment.anchor.startLine || resolved[1] !== comment.anchor.endLine)) {
			out.push({ id: comment.id, startLine: resolved[0], endLine: resolved[1] });
		}
	}
	return out;
}
